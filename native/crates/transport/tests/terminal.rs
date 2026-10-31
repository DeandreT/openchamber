//! The terminal transport against scripted loopback servers: the socket's
//! bearer upgrade, snapshot and sequence handling, reconnect with snapshot
//! replay, input split under the server's frame limit, idle close, refused
//! credentials and the HTTP command plane's request fidelity and refusals.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::terminal::{
    CreateTerminal, MAX_FRAME_BYTES, SocketTiming, TerminalApi, TerminalEvent, TerminalSocket,
    TerminalStatus, ThemeMode,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

const WAIT: Duration = Duration::from_secs(10);

fn fast_timing() -> SocketTiming {
    SocketTiming {
        open_timeout: Duration::from_secs(2),
        backoff_base: Duration::from_millis(50),
        backoff_cap: Duration::from_millis(400),
        refused_delay: Duration::from_millis(900),
        keepalive: Duration::from_secs(30),
        idle_grace: Duration::from_millis(300),
    }
}

fn frame(value: Value) -> Message {
    let mut bytes = vec![1u8];
    bytes.extend(serde_json::to_vec(&value).expect("json"));
    Message::Binary(bytes.into())
}

fn parse(message: &Message) -> Value {
    let Message::Binary(bytes) = message else {
        panic!("the client sends binary frames only, got {message:?}");
    };
    assert_eq!(bytes[0], 1, "every client frame starts with the tag");
    serde_json::from_slice(&bytes[1..]).expect("the payload is JSON")
}

type WsServer = tokio_tungstenite::WebSocketStream<TcpStream>;

/// What the scripted socket server saw: one entry per accepted connection.
#[derive(Default)]
struct Seen {
    authorization: Vec<Option<String>>,
}

#[expect(
    clippy::result_large_err,
    reason = "the handshake callback's error type is tungstenite's `ErrorResponse`"
)]
async fn upgrade(stream: TcpStream, recorded: Arc<Mutex<Seen>>) -> WsServer {
    // The server's `maxPayload`: a larger frame fails the socket.
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES));
    let callback = move |request: &Request, response: Response| {
        assert_eq!(request.uri().path(), "/api/terminal/ws");
        assert_eq!(
            request
                .headers()
                .get("origin")
                .and_then(|value| value.to_str().ok()),
            request
                .headers()
                .get("host")
                .and_then(|value| value.to_str().ok())
                .map(|host| format!("http://{host}"))
                .as_deref()
        );
        let header = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        recorded.lock().expect("seen").authorization.push(header);
        Ok(response)
    };
    tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(config))
        .await
        .expect("upgrade")
}

/// Accepts upgrades and hands each socket, in order, to the test.
async fn socket_server(
    refuse_first: usize,
) -> (
    Endpoint,
    mpsc::UnboundedReceiver<WsServer>,
    Arc<Mutex<Seen>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (sender, receiver) = mpsc::unbounded_channel();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let recorded = Arc::clone(&seen);
    tokio::spawn(async move {
        let mut accepted = 0usize;
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            accepted += 1;
            if accepted <= refuse_first {
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n")
                    .await;
                continue;
            }
            let socket = upgrade(stream, Arc::clone(&recorded)).await;
            if sender.send(socket).is_err() {
                return;
            }
        }
    });
    let endpoint = Endpoint::parse(&format!("http://{address}")).expect("endpoint");
    (endpoint, receiver, seen)
}

async fn next_frame(socket: &mut WsServer) -> Value {
    loop {
        let message = tokio::time::timeout(WAIT, socket.next())
            .await
            .expect("the client sends a frame")
            .expect("the socket stays open")
            .expect("a valid frame");
        if matches!(message, Message::Binary(_)) {
            return parse(&message);
        }
    }
}

async fn next_event(socket: &mut TerminalSocket) -> TerminalEvent {
    tokio::time::timeout(WAIT, socket.next())
        .await
        .expect("an event arrives")
        .expect("the socket task runs")
}

fn snapshot(session: &str, sequence: u64, history: &str) -> Message {
    frame(json!({
        "t": "snapshot", "v": 3, "s": session, "q": sequence, "history": history,
        "cols": 120, "rows": 32, "status": "running", "exitCode": null, "signal": null,
        "mode": "interactive", "purpose": {"type": "terminal"}, "runtime": "node", "ptyBackend": "node-pty",
    }))
}

fn output(session: &str, sequence: u64, data: &str) -> Message {
    frame(json!({ "t": "output", "v": 3, "s": session, "q": sequence, "d": data }))
}

#[tokio::test]
async fn attach_sends_the_bearer_and_delivers_each_sequence_once() {
    let (endpoint, mut sockets, seen) = socket_server(0).await;
    let client = ServerClient::new(
        endpoint,
        Credential::ClientToken("oc_client_terminal".to_owned()),
    )
    .expect("client");
    let mut socket = TerminalSocket::start_with(client, fast_timing());
    socket.commands().attach("t1");

    let mut server = tokio::time::timeout(WAIT, sockets.recv())
        .await
        .expect("the client dials on attach")
        .expect("a socket");
    assert_eq!(next_frame(&mut server).await, json!({"t": "hello", "v": 3}));
    assert_eq!(
        next_frame(&mut server).await,
        json!({"t": "attach", "v": 3, "s": "t1"})
    );
    assert_eq!(
        seen.lock().expect("seen").authorization,
        vec![Some("Bearer oc_client_terminal".to_owned())]
    );
    assert_eq!(next_event(&mut socket).await, TerminalEvent::Connected);

    server
        .send(frame(json!({"t": "hello", "v": 3})))
        .await
        .expect("send");
    server
        .send(output("t1", 2, "before snapshot"))
        .await
        .expect("send");
    server.send(snapshot("t1", 3, "$ ")).await.expect("send");
    server
        .send(output("t1", 3, "covered by the snapshot"))
        .await
        .expect("send");
    server.send(output("t1", 4, "ls\r\n")).await.expect("send");
    server.send(output("t1", 4, "repeat")).await.expect("send");
    server
        .send(output("t2", 9, "not attached"))
        .await
        .expect("send");
    server
        .send(frame(
            json!({"t": "exit", "v": 3, "s": "t1", "q": 5, "exitCode": 3, "signal": null}),
        ))
        .await
        .expect("send");

    // Output before the first snapshot passes: the reference's subscriber
    // starts at -1. Everything at or below the snapshot afterwards is dropped.
    assert_eq!(
        next_event(&mut socket).await,
        TerminalEvent::Output {
            session: "t1".to_owned(),
            sequence: 2,
            data: "before snapshot".to_owned()
        }
    );
    let TerminalEvent::Snapshot(first) = next_event(&mut socket).await else {
        panic!("a snapshot follows");
    };
    assert_eq!((first.sequence, first.history.as_str()), (3, "$ "));
    assert_eq!((first.cols, first.rows), (Some(120), Some(32)));
    assert_eq!(first.status, TerminalStatus::Running);
    assert_eq!(
        next_event(&mut socket).await,
        TerminalEvent::Output {
            session: "t1".to_owned(),
            sequence: 4,
            data: "ls\r\n".to_owned()
        }
    );
    assert_eq!(
        next_event(&mut socket).await,
        TerminalEvent::Exit {
            session: "t1".to_owned(),
            exit_code: Some(3),
            signal: None
        }
    );
}

#[tokio::test]
async fn a_dropped_socket_reconnects_and_resumes_from_a_fresh_snapshot() {
    let (endpoint, mut sockets, _) = socket_server(0).await;
    let client = ServerClient::new(endpoint, Credential::None).expect("client");
    let mut socket = TerminalSocket::start_with(client, fast_timing());
    socket.commands().attach("t1");

    let mut first = sockets.recv().await.expect("a socket");
    next_frame(&mut first).await;
    next_frame(&mut first).await;
    assert_eq!(next_event(&mut socket).await, TerminalEvent::Connected);
    first.send(snapshot("t1", 1, "1\r\n")).await.expect("send");
    first.send(output("t1", 2, "2\r\n")).await.expect("send");
    assert!(matches!(
        next_event(&mut socket).await,
        TerminalEvent::Snapshot(_)
    ));
    assert!(matches!(
        next_event(&mut socket).await,
        TerminalEvent::Output { sequence: 2, .. }
    ));

    // The connection drops mid-output.
    drop(first);
    let started = tokio::time::Instant::now();
    assert_eq!(
        next_event(&mut socket).await,
        TerminalEvent::Reconnecting {
            attempt: 1,
            delay: Duration::from_millis(50),
            refused: false
        }
    );
    let mut second = tokio::time::timeout(WAIT, sockets.recv())
        .await
        .expect("the client reconnects")
        .expect("a socket");
    assert!(
        started.elapsed() >= Duration::from_millis(50),
        "the backoff was waited out"
    );
    assert_eq!(next_frame(&mut second).await, json!({"t": "hello", "v": 3}));
    assert_eq!(
        next_frame(&mut second).await,
        json!({"t": "attach", "v": 3, "s": "t1"})
    );
    assert_eq!(next_event(&mut socket).await, TerminalEvent::Connected);

    // The server's snapshot covers what the client missed while away; output
    // it had already delivered and the snapshot covers is not repeated.
    second
        .send(snapshot("t1", 4, "1\r\n2\r\n3\r\n4\r\n"))
        .await
        .expect("send");
    second.send(output("t1", 3, "3\r\n")).await.expect("send");
    second.send(output("t1", 5, "5\r\n")).await.expect("send");
    let TerminalEvent::Snapshot(resumed) = next_event(&mut socket).await else {
        panic!("the reconnect replays a snapshot");
    };
    assert_eq!(resumed.history, "1\r\n2\r\n3\r\n4\r\n");
    assert_eq!(
        next_event(&mut socket).await,
        TerminalEvent::Output {
            session: "t1".to_owned(),
            sequence: 5,
            data: "5\r\n".to_owned()
        }
    );

    // Input works on the new socket.
    socket.commands().write("t1", "echo ok\r".to_owned());
    assert_eq!(
        next_frame(&mut second).await,
        json!({"t": "write", "v": 3, "s": "t1", "d": "echo ok\r"})
    );
}

#[tokio::test]
async fn a_large_paste_arrives_complete_and_in_order_under_the_frame_limit() {
    let (endpoint, mut sockets, _) = socket_server(0).await;
    let client = ServerClient::new(endpoint, Credential::None).expect("client");
    let mut socket = TerminalSocket::start_with(client, fast_timing());
    socket.commands().attach("t1");
    let mut server = sockets.recv().await.expect("a socket");
    next_frame(&mut server).await;
    next_frame(&mut server).await;
    assert_eq!(next_event(&mut socket).await, TerminalEvent::Connected);

    let mut paste = String::from("\u{1b}[200~");
    let mut line = 0;
    while paste.len() < 200 * 1024 {
        paste.push_str(&format!("line {line} 好\u{1f600}\t\"quoted\"\n"));
        line += 1;
    }
    paste.push_str("\u{1b}[201~");
    socket.commands().write("t1", paste.clone());
    socket.commands().write("t1", "after\r".to_owned());

    let mut received = String::new();
    let mut frames = 0;
    while !received.ends_with("after\r") {
        let value = next_frame(&mut server).await;
        assert_eq!(value["t"], "write");
        assert_eq!(value["s"], "t1");
        received.push_str(value["d"].as_str().expect("data"));
        frames += 1;
    }
    assert!(frames > 4, "the paste was split, {frames} frames");
    assert_eq!(received, format!("{paste}after\r"));
    // The socket survived every frame: a ping still gets through.
    server
        .send(frame(json!({"t": "pong", "v": 3})))
        .await
        .expect("socket open");
}

#[tokio::test]
async fn input_lost_to_a_failed_socket_is_reported() {
    let (endpoint, mut sockets, _) = socket_server(0).await;
    let client = ServerClient::new(endpoint, Credential::None).expect("client");
    let mut socket = TerminalSocket::start_with(client, fast_timing());
    socket.commands().attach("t1");
    let mut server = sockets.recv().await.expect("a socket");
    next_frame(&mut server).await;
    next_frame(&mut server).await;
    assert_eq!(next_event(&mut socket).await, TerminalEvent::Connected);
    server.close(None).await.expect("close");
    drop(server);
    // Stop accepting, so the next dial fails while input waits.
    drop(sockets);
    assert!(matches!(
        next_event(&mut socket).await,
        TerminalEvent::Reconnecting { .. }
    ));
    socket.commands().write("t1", "lost\r".to_owned());
    loop {
        match next_event(&mut socket).await {
            TerminalEvent::WriteFailed { session } => {
                assert_eq!(session, "t1");
                break;
            }
            TerminalEvent::Reconnecting { .. } | TerminalEvent::Connected => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn the_socket_closes_after_the_idle_grace_and_redials_on_attach() {
    let (endpoint, mut sockets, _) = socket_server(0).await;
    let client = ServerClient::new(endpoint, Credential::None).expect("client");
    let mut socket = TerminalSocket::start_with(client, fast_timing());
    let commands = socket.commands();
    commands.attach("t1");
    let mut server = sockets.recv().await.expect("a socket");
    next_frame(&mut server).await;
    next_frame(&mut server).await;
    assert_eq!(next_event(&mut socket).await, TerminalEvent::Connected);

    // A detach and a quick reattach reuse the open socket.
    commands.detach("t1");
    assert_eq!(
        next_frame(&mut server).await,
        json!({"t": "detach", "v": 3, "s": "t1"})
    );
    commands.attach("t1");
    assert_eq!(
        next_frame(&mut server).await,
        json!({"t": "attach", "v": 3, "s": "t1"})
    );
    commands.detach("t1");
    next_frame(&mut server).await;
    // Past the grace the client closes, and does not reconnect by itself.
    let closed = tokio::time::timeout(WAIT, async {
        loop {
            match server.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => continue,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the idle socket closes");
    assert!(
        tokio::time::timeout(Duration::from_millis(500), sockets.recv())
            .await
            .is_err(),
        "nothing redials while no terminal is attached"
    );
    commands.attach("t2");
    let mut again = tokio::time::timeout(WAIT, sockets.recv())
        .await
        .expect("an attach redials")
        .expect("a socket");
    next_frame(&mut again).await;
    assert_eq!(
        next_frame(&mut again).await,
        json!({"t": "attach", "v": 3, "s": "t2"})
    );
}

#[tokio::test]
async fn refused_credentials_wait_the_long_delay() {
    let (endpoint, mut sockets, _) = socket_server(1).await;
    let client =
        ServerClient::new(endpoint, Credential::ClientToken("stale".to_owned())).expect("client");
    let mut socket = TerminalSocket::start_with(client, fast_timing());
    socket.commands().attach("t1");
    assert_eq!(
        next_event(&mut socket).await,
        TerminalEvent::Reconnecting {
            attempt: 1,
            delay: Duration::from_millis(900),
            refused: true
        }
    );
    let started = tokio::time::Instant::now();
    let mut server = tokio::time::timeout(WAIT, sockets.recv())
        .await
        .expect("the client retries")
        .expect("a socket");
    assert!(started.elapsed() >= Duration::from_millis(800));
    next_frame(&mut server).await;
    assert_eq!(next_event(&mut socket).await, TerminalEvent::Connected);
}

/// One HTTP request the command server received.
#[derive(Clone, Debug)]
struct Recorded {
    request_line: String,
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
}

/// Answers every request with the status and JSON body the handler picks.
async fn command_server(
    handler: impl Fn(&Recorded) -> (u16, Value) + Send + Sync + 'static,
) -> (Endpoint, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let recorded = Arc::clone(&recorded);
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end = loop {
                    let read = stream.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
                let mut lines = head.split("\r\n");
                let request_line = lines.next().unwrap_or_default().to_owned();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
                    .collect();
                let length: usize = headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.parse().ok())
                    .unwrap_or(0);
                while buffer.len() < header_end + length {
                    let read = stream.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                }
                let request = Recorded {
                    request_line,
                    headers,
                    body: buffer[header_end..].to_vec(),
                };
                let (status, body) = handler(&request);
                recorded.lock().expect("requests").push(request);
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    let endpoint = Endpoint::parse(&format!("http://{address}")).expect("endpoint");
    (endpoint, requests)
}

fn create_request(cwd: &str) -> CreateTerminal {
    CreateTerminal {
        cwd: cwd.to_owned(),
        cols: 100,
        rows: 30,
        theme_mode: ThemeMode::Dark,
        background: Some("#151313".to_owned()),
        foreground: Some("#cdccc3".to_owned()),
        shell: "auto".to_owned(),
        login_shell: false,
    }
}

#[tokio::test]
async fn commands_carry_the_directory_and_bodies_the_server_reads() {
    let (endpoint, requests) = command_server(|request| {
        if request.request_line.starts_with("POST /api/terminal/create") {
            (200, json!({"sessionId": "t1", "cols": 100, "rows": 30, "status": "running", "mode": "interactive", "purpose": {"type": "terminal"}}))
        } else if request.request_line.starts_with("GET /api/terminal/shells") {
            (200, json!([{"id": "zsh", "name": "zsh", "supportsLogin": true}, {"id": 4}]))
        } else if request.request_line.starts_with("GET /api/terminal/sessions") {
            (200, json!({"sessions": [
                {"sessionId": "t1", "cwd": "/work/my app", "status": "running", "createdAt": 5, "mode": "interactive", "purpose": {"type": "terminal"}},
                {"sessionId": "bad", "cwd": 3},
                {"sessionId": "t2", "cwd": "/work/my app", "status": "exited", "createdAt": null, "mode": "command", "purpose": {"type": "project-action", "actionId": "dev", "executionId": "e1"}}
            ]}))
        } else {
            (200, json!({"success": true}))
        }
    })
    .await;
    let api = TerminalApi::new(
        ServerClient::new(
            endpoint,
            Credential::ClientToken("oc_client_terminal".to_owned()),
        )
        .expect("client"),
    );
    let created = api
        .create(&create_request("/work/my app"))
        .await
        .expect("create");
    assert_eq!(created.session_id, "t1");
    api.resize("t1", 132, 40, "/work/my app")
        .await
        .expect("resize");
    api.close("t1", "/work/my app").await.expect("close");
    let sessions = api.sessions("/work/my app").await.expect("list");
    assert_eq!(sessions.len(), 2, "the malformed entry is left out");
    assert!(sessions[0].interactive && sessions[0].status == TerminalStatus::Running);
    assert!(!sessions[1].interactive && sessions[1].status == TerminalStatus::Exited);
    assert!(
        api.shells().await.is_err(),
        "a malformed shell list is an error, never an empty one"
    );

    let requests = requests.lock().expect("requests").clone();
    let create = &requests[0];
    assert!(
        create
            .request_line
            .starts_with("POST /api/terminal/create ")
    );
    assert_eq!(
        create.header("authorization"),
        Some("Bearer oc_client_terminal")
    );
    assert_eq!(
        create.header("x-opencode-directory"),
        Some("%2Fwork%2Fmy%20app")
    );
    let body: Value = serde_json::from_slice(&create.body).expect("json");
    assert_eq!(
        body,
        json!({
            "cwd": "/work/my app", "cols": 100, "rows": 30, "themeMode": "dark",
            "terminalBackground": "#151313", "terminalForeground": "#cdccc3",
            "shell": "auto", "loginShell": false,
        })
    );
    assert!(
        requests[1]
            .request_line
            .starts_with("POST /api/terminal/t1/resize ")
    );
    let body: Value = serde_json::from_slice(&requests[1].body).expect("json");
    assert_eq!(body, json!({"cols": 132, "rows": 40}));
    assert!(
        requests[2]
            .request_line
            .starts_with("DELETE /api/terminal/t1 ")
    );
    assert!(
        requests[3]
            .request_line
            .starts_with("GET /api/terminal/sessions?cwd=%2Fwork%2Fmy+app ")
    );
}

#[tokio::test]
async fn refusals_keep_the_server_message_and_code() {
    let (endpoint, _) = command_server(|request| {
        if request.request_line.contains("/api/terminal/create") {
            (
                400,
                json!({"error": "Invalid working directory", "code": "TERMINAL_CWD_MISSING"}),
            )
        } else {
            (429, json!({"error": "Maximum terminal sessions reached"}))
        }
    })
    .await;
    let api = TerminalApi::new(ServerClient::new(endpoint, Credential::None).expect("client"));
    let missing = api
        .create(&create_request("/gone"))
        .await
        .expect_err("a deleted directory is refused");
    assert!(missing.is_cwd_missing());
    let full = api
        .restart("t1", &create_request("/work"))
        .await
        .expect_err("refused");
    assert!(!full.is_cwd_missing());
    assert!(
        full.to_string()
            .contains("Maximum terminal sessions reached")
    );
}
