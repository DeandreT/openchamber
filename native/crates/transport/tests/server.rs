//! The transport against a scripted local server: authentication, request
//! fidelity, failure classification, paging, cancellation and the event
//! stream's resume, reset, fragmentation and timeout behavior.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use openchamber_native_transport::client::{ClientDescription, ServerClient};
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::ServerEvent;
use openchamber_native_transport::stream::{EventStream, StreamItem};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};

/// One request the server received.
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("the request body is JSON")
    }
}

/// An HTTP answer.
#[derive(Clone, Debug)]
struct Reply {
    status: u16,
    content_type: &'static str,
    headers: Vec<(&'static str, String)>,
    body: String,
    delay: Duration,
}

impl Reply {
    fn json(value: Value) -> Self {
        Self::with_status(200, "application/json", value.to_string())
    }

    fn with_status(status: u16, content_type: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type,
            headers: Vec::new(),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }
}

/// What a WebSocket connection does, in order.
#[derive(Clone, Debug)]
enum WsStep {
    Text(String),
    /// A text message split into a start frame and a continuation frame at
    /// a byte offset, which may fall inside a character.
    Fragmented(String, usize),
    Wait(Duration),
    Close,
}

type HttpHandler = dyn Fn(&Recorded) -> Reply + Send + Sync;

/// A scripted server on a loopback port.
struct Server {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
    /// HTTP requests whose client went away before the reply.
    abandoned: Arc<Mutex<usize>>,
    _task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(
        http: impl Fn(&Recorded) -> Reply + Send + Sync + 'static,
        websocket: Vec<Vec<WsStep>>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let address = listener.local_addr().expect("a bound address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let abandoned = Arc::new(Mutex::new(0));
        let http: Arc<HttpHandler> = Arc::new(http);
        let scripts = Arc::new(Mutex::new(VecDeque::from(websocket)));
        let task = {
            let requests = Arc::clone(&requests);
            let abandoned = Arc::clone(&abandoned);
            tokio::spawn(async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        return;
                    };
                    let requests = Arc::clone(&requests);
                    let abandoned = Arc::clone(&abandoned);
                    let http = Arc::clone(&http);
                    let scripts = Arc::clone(&scripts);
                    tokio::spawn(async move {
                        let mut head = [0u8; 512];
                        let peeked = socket.peek(&mut head).await.unwrap_or(0);
                        let line = String::from_utf8_lossy(&head[..peeked]).to_lowercase();
                        if line.contains("upgrade: websocket") {
                            let steps = scripts.lock().expect("unpoisoned").pop_front();
                            serve_websocket(socket, steps.unwrap_or_default(), requests).await;
                        } else {
                            serve_http(socket, http, requests, abandoned).await;
                        }
                    });
                }
            })
        };
        Self {
            address,
            requests,
            abandoned,
            _task: task,
        }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::parse(&format!("http://{}/prefix", self.address)).expect("a valid endpoint")
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("unpoisoned").clone()
    }
}

async fn serve_http(
    mut socket: TcpStream,
    http: Arc<HttpHandler>,
    requests: Arc<Mutex<Vec<Recorded>>>,
    abandoned: Arc<Mutex<usize>>,
) {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let Some(request) = read_request(&mut socket, &mut buffer).await else {
            return;
        };
        requests.lock().expect("unpoisoned").push(request.clone());
        let reply = http(&request);
        if !reply.delay.is_zero() {
            let mut probe = [0u8; 1];
            tokio::select! {
                () = tokio::time::sleep(reply.delay) => {}
                read = socket.read(&mut probe) => {
                    if matches!(read, Ok(0) | Err(_)) {
                        *abandoned.lock().expect("unpoisoned") += 1;
                        return;
                    }
                }
            }
        }
        let mut head = format!(
            "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\n",
            reply.status,
            reply.content_type,
            reply.body.len()
        );
        for (name, value) in &reply.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        if socket.write_all(head.as_bytes()).await.is_err()
            || socket.write_all(reply.body.as_bytes()).await.is_err()
        {
            return;
        }
    }
}

/// Reads one request; `None` when the client closed the connection.
async fn read_request(socket: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<Recorded> {
    let header_end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        let mut chunk = [0u8; 4096];
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_owned();
    let target = request_line.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while buffer.len() < body_start + length {
        let mut chunk = [0u8; 4096];
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = buffer[body_start..body_start + length].to_vec();
    buffer.drain(..body_start + length);
    Some(Recorded {
        method,
        target,
        headers,
        body,
    })
}

#[expect(
    clippy::result_large_err,
    reason = "the handshake callback's error type is tungstenite's `ErrorResponse`"
)]
async fn serve_websocket(
    socket: TcpStream,
    steps: Vec<WsStep>,
    requests: Arc<Mutex<Vec<Recorded>>>,
) {
    let recorded = Arc::new(Mutex::new(None));
    let capture = Arc::clone(&recorded);
    let accepted = tokio_tungstenite::accept_hdr_async(
        socket,
        move |request: &Request, response: Response| {
            let headers = request
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        value.to_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect();
            *capture.lock().expect("unpoisoned") = Some(Recorded {
                method: "GET".to_owned(),
                target: request.uri().to_string(),
                headers,
                body: Vec::new(),
            });
            Ok(response)
        },
    )
    .await;
    if let Some(request) = recorded.lock().expect("unpoisoned").take() {
        requests.lock().expect("unpoisoned").push(request);
    }
    let Ok(mut socket) = accepted else {
        return;
    };
    for step in steps {
        let sent = match step {
            WsStep::Text(text) => socket.send(Message::text(text)).await,
            WsStep::Fragmented(text, split) => {
                let bytes = text.into_bytes();
                let (first, rest) = bytes.split_at(split);
                let start = Frame::message(first.to_vec(), OpCode::Data(Data::Text), false);
                let end = Frame::message(rest.to_vec(), OpCode::Data(Data::Continue), true);
                match socket.send(Message::Frame(start)).await {
                    Ok(()) => socket.send(Message::Frame(end)).await,
                    Err(error) => Err(error),
                }
            }
            WsStep::Wait(duration) => {
                tokio::time::sleep(duration).await;
                Ok(())
            }
            WsStep::Close => {
                let _ = socket.close(None).await;
                return;
            }
        };
        if sent.is_err() {
            return;
        }
    }
    // Hold the connection open until the client leaves.
    while socket.next().await.is_some() {}
}

fn health() -> Value {
    json!({
        "status": "ok",
        "openchamberVersion": "2.4.0",
        "runtime": "web",
        "compatibility": {
            "apiVersion": 1,
            "minClientApiVersion": 1,
            "capabilities": ["realtime.websocket.global-events.v1", "api.message-part-shells.v1"],
        },
    })
}

fn session(id: &str) -> Value {
    json!({
        "id": id,
        "location": {"directory": "/home/synthetic/project"},
        "title": id,
        "time": {"created": 1, "updated": 2},
    })
}

fn description() -> ClientDescription {
    ClientDescription {
        label: "OpenChamber Desktop".to_owned(),
        kind: "desktop".to_owned(),
        dedupe_key: Some("desktop:0123".to_owned()),
        device_platform: Some("linux".to_owned()),
        app_version: Some("0.1.0".to_owned()),
    }
}

#[tokio::test]
async fn login_describes_the_client_and_requests_carry_the_bearer_token() {
    let server = Server::start(
        |request| match request.target.as_str() {
            "/prefix/auth/session" => {
                Reply::json(json!({"authenticated": true, "clientToken": "oc_client_abc"}))
            }
            _ => Reply::json(json!({"data": [], "cursor": {}})),
        },
        Vec::new(),
    )
    .await;
    let anonymous = ServerClient::new(server.endpoint(), Credential::None).expect("a client");
    let credential = anonymous
        .login("hunter2", &description())
        .await
        .expect("the login succeeds");
    let client = ServerClient::new(server.endpoint(), credential).expect("a client");
    client.session_page(None).await.expect("the page reads");

    let requests = server.requests();
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].json(),
        json!({
            "password": "hunter2",
            "trustDevice": true,
            "issueClientToken": true,
            "clientLabel": "OpenChamber Desktop",
            "clientKind": "desktop",
            "dedupeKey": "desktop:0123",
            "devicePlatform": "linux",
            "appVersion": "0.1.0",
        })
    );
    assert_eq!(requests[1].target, "/prefix/api/session?limit=500");
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer oc_client_abc")
    );
    assert_eq!(requests[1].header("x-opencode-directory"), None);
}

#[tokio::test]
async fn login_failures_are_distinct() {
    let server = Server::start(
        |request| match request.json()["password"].as_str() {
            Some("wrong") => Reply::with_status(
                401,
                "application/json",
                r#"{"error":"Invalid credentials"}"#,
            ),
            _ => Reply::json(json!({"authenticated": true})),
        },
        Vec::new(),
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("a client");
    assert!(matches!(
        client.login("wrong", &description()).await,
        Err(TransportError::AuthenticationRejected)
    ));
    assert!(
        matches!(
            client.login("right", &description()).await,
            Err(TransportError::Malformed(_))
        ),
        "a login that issued no token is not a success"
    );
}

#[tokio::test]
async fn the_identity_probe_sends_no_credential_and_classifies_servers() {
    let server = Server::start(
        |request| match request.target.as_str() {
            "/prefix/health" => Reply::json(health()),
            _ => Reply::with_status(200, "text/html", "<html></html>"),
        },
        Vec::new(),
    )
    .await;
    let client = ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("oc_client_x".to_owned()),
    )
    .expect("a client");
    let identity = client.probe().await.expect("an OpenChamber server");
    assert_eq!(identity.openchamber_version, "2.4.0");
    assert_eq!(server.requests()[0].header("authorization"), None);

    let foreign = Server::start(
        |_| Reply::with_status(200, "text/html", "<html></html>"),
        Vec::new(),
    )
    .await;
    let client = ServerClient::new(foreign.endpoint(), Credential::None).expect("a client");
    assert!(matches!(
        client.probe().await,
        Err(TransportError::NotOpenChamber)
    ));

    let newer = Server::start(
        |_| {
            let mut health = health();
            health["compatibility"]["minClientApiVersion"] = json!(9);
            Reply::json(health)
        },
        Vec::new(),
    )
    .await;
    let client = ServerClient::new(newer.endpoint(), Credential::None).expect("a client");
    assert!(matches!(
        client.probe().await,
        Err(TransportError::Incompatible(_))
    ));
}

#[tokio::test]
async fn redirects_are_never_followed_and_failures_never_read_as_empty() {
    let server = Server::start(
        |request| match request.target.as_str() {
            target if target.starts_with("/prefix/api/session?") => {
                let mut reply = Reply::with_status(302, "text/plain", "");
                reply
                    .headers
                    .push(("location", "http://127.0.0.1:9/elsewhere".to_owned()));
                reply
            }
            "/prefix/api/fs/home" => Reply::with_status(200, "text/html", "<html></html>"),
            _ => Reply::with_status(401, "application/json", "{}"),
        },
        Vec::new(),
    )
    .await;
    let client = ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("oc_client_x".to_owned()),
    )
    .expect("a client");
    assert!(matches!(
        client.session_page(None).await,
        Err(TransportError::Redirected)
    ));
    assert_eq!(
        server.requests().len(),
        1,
        "the redirect target was never asked"
    );
    assert!(matches!(
        client.chat_roots().await,
        Err(TransportError::Malformed(_))
    ));
    assert!(matches!(
        client.settings().await,
        Err(TransportError::AuthenticationRequired)
    ));
}

#[tokio::test]
async fn a_failed_page_fails_the_list_and_a_repeated_cursor_stops_paging() {
    let failing = Server::start(
        |request| {
            if request.target.contains("cursor=") {
                Reply::with_status(500, "application/json", "{}")
            } else {
                Reply::json(json!({"data": [session("a")], "cursor": {"next": "c1"}}))
            }
        },
        Vec::new(),
    )
    .await;
    let client = ServerClient::new(failing.endpoint(), Credential::None).expect("a client");
    assert!(matches!(
        client.all_sessions().await,
        Err(TransportError::Http { status: 500 })
    ));

    let looping = Server::start(
        |_| Reply::json(json!({"data": [session("a"), session("a")], "cursor": {"next": "c1"}})),
        Vec::new(),
    )
    .await;
    let client = ServerClient::new(looping.endpoint(), Credential::None).expect("a client");
    let sessions = client.all_sessions().await.expect("the list reads");
    assert_eq!(sessions.len(), 1, "duplicates are dropped");
    assert_eq!(
        looping.requests().len(),
        2,
        "a page with nothing new ends paging"
    );
}

#[tokio::test]
async fn dropping_a_request_cancels_it() {
    let server = Server::start(
        |_| {
            let mut reply = Reply::json(json!({"data": [], "cursor": {}}));
            reply.delay = Duration::from_secs(10);
            reply
        },
        Vec::new(),
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("a client");
    let outcome = tokio::time::timeout(Duration::from_millis(200), client.session_page(None)).await;
    assert!(outcome.is_err(), "the request was still waiting");
    let mut waited = Duration::ZERO;
    while *server.abandoned.lock().expect("unpoisoned") == 0 && waited < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }
    assert_eq!(
        *server.abandoned.lock().expect("unpoisoned"),
        1,
        "the connection closed"
    );
}

fn ready(reset: bool) -> WsStep {
    WsStep::Text(if reset {
        r#"{"type":"ready","scope":"global","replayReset":true}"#.to_owned()
    } else {
        r#"{"type":"ready","scope":"global"}"#.to_owned()
    })
}

fn renamed(event_id: &str, title: &str) -> String {
    json!({
        "type": "event",
        "eventId": event_id,
        "payload": {
            "id": event_id,
            "created": 5,
            "type": "session.renamed",
            "data": {"sessionID": "s1", "title": title},
        },
    })
    .to_string()
}

async fn next_item(stream: &mut EventStream) -> StreamItem {
    tokio::time::timeout(Duration::from_secs(15), stream.next())
        .await
        .expect("the stream delivers in time")
        .expect("the stream is open")
}

fn last_event_id(request: &Recorded) -> Option<String> {
    let query = request.target.split_once('?')?.1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("lastEventId="))
        .map(str::to_owned)
}

#[tokio::test]
async fn the_stream_resumes_from_the_last_event_and_retires_a_reset_cursor() {
    let server = Server::start(
        |_| Reply::with_status(404, "text/plain", ""),
        vec![
            vec![
                ready(false),
                WsStep::Text(renamed("e1", "one")),
                WsStep::Close,
            ],
            vec![ready(false), WsStep::Close],
            vec![ready(true), WsStep::Close],
            vec![ready(false)],
        ],
    )
    .await;
    let client = ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("oc_client_ws".to_owned()),
    )
    .expect("a client");
    let mut stream = EventStream::subscribe(client);
    let mut connected = Vec::new();
    while connected.len() < 4 {
        if let StreamItem::Connected { resumed } = next_item(&mut stream).await {
            connected.push(resumed);
        }
    }
    assert_eq!(
        connected,
        [false, true, false, false],
        "only a replay from a known cursor resumes"
    );
    let upgrades: Vec<Recorded> = server
        .requests()
        .into_iter()
        .filter(|request| request.target.starts_with("/prefix/api/global/event/ws"))
        .collect();
    let cursors: Vec<Option<String>> = upgrades.iter().map(last_event_id).collect();
    assert_eq!(
        cursors,
        [None, Some("e1".to_owned()), Some("e1".to_owned()), None],
        "a reset retires the cursor"
    );
    assert!(
        upgrades
            .iter()
            .all(|request| request.header("authorization") == Some("Bearer oc_client_ws")),
        "every upgrade carries the credential"
    );
}

#[tokio::test]
async fn fragmented_frames_decode_across_a_split_character() {
    let message = renamed("e1", "Grüße 🎉");
    let split = message.find('ü').expect("the title is in the frame") + 1;
    let server = Server::start(
        |_| Reply::with_status(404, "text/plain", ""),
        vec![vec![ready(false), WsStep::Fragmented(message, split)]],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("a client");
    let mut stream = EventStream::subscribe(client);
    loop {
        if let StreamItem::Events(events) = next_item(&mut stream).await {
            let title = events.iter().find_map(|event| match event {
                ServerEvent::SessionPatched { patch, .. } => patch.title.clone(),
                _ => None,
            });
            assert_eq!(title.as_deref(), Some("Grüße 🎉"));
            return;
        }
    }
}

#[tokio::test]
async fn a_socket_that_never_says_ready_is_replaced() {
    let server = Server::start(
        |_| Reply::with_status(404, "text/plain", ""),
        vec![
            vec![WsStep::Wait(Duration::from_secs(30))],
            vec![ready(false)],
        ],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("a client");
    let mut stream = EventStream::subscribe(client);
    let first = next_item(&mut stream).await;
    assert!(
        matches!(first, StreamItem::Disconnected(TransportError::Stream(_))),
        "{first:?}"
    );
    let second = next_item(&mut stream).await;
    assert!(
        matches!(second, StreamItem::Connected { resumed: false }),
        "{second:?}"
    );
}
