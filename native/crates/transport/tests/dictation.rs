//! The dictation stream against a scripted loopback server: the bearer
//! upgrade, chunk numbering, the transcript after finish, a start retried
//! while the model downloads (chunks sent again from the first), a failed
//! dictation and a cancel when the recording goes away.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::dictation::{DictationStream, StartOptions, StreamEvent, Timing};
use openchamber_native_transport::endpoint::Endpoint;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

const WAIT: Duration = Duration::from_secs(10);

type WsServer = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

fn timing() -> Timing {
    Timing {
        connect: Duration::from_secs(2),
        start: Duration::from_secs(2),
        finish: Duration::from_secs(2),
        finish_margin: Duration::from_millis(100),
        download_retry: Duration::from_millis(50),
    }
}

/// Hands each upgraded socket to the test with its authorization header.
async fn server() -> (
    Endpoint,
    mpsc::UnboundedReceiver<(WsServer, Option<String>)>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let mut authorization = None;
            let socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |request: &Request, response: Response| {
                    assert_eq!(request.uri().path(), "/api/dictation/ws");
                    authorization = request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    Ok(response)
                },
            )
            .await
            .expect("upgrade");
            if sender.send((socket, authorization)).is_err() {
                break;
            }
        }
    });
    (
        Endpoint::parse(&format!("http://{address}")).expect("endpoint"),
        receiver,
    )
}

async fn send(socket: &mut WsServer, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .expect("server send");
}

async fn next(socket: &mut WsServer) -> Value {
    loop {
        let message = tokio::time::timeout(WAIT, socket.next())
            .await
            .expect("a client frame")
            .expect("open")
            .expect("frame");
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).expect("json frame");
        }
    }
}

async fn event(events: &mut mpsc::UnboundedReceiver<StreamEvent>) -> StreamEvent {
    tokio::time::timeout(WAIT, events.recv())
        .await
        .expect("an event")
        .expect("open stream")
}

fn client(endpoint: Endpoint) -> ServerClient {
    ServerClient::new(
        endpoint,
        Credential::ClientToken("oc_client_fixture".into()),
    )
    .expect("client")
}

#[tokio::test]
async fn chunks_are_numbered_and_finish_returns_the_transcript() {
    let (endpoint, mut sockets) = server().await;
    let (stream, mut events) = DictationStream::open(
        client(endpoint),
        StartOptions {
            local_model: "parakeet-tdt-0.6b-v3-int8".into(),
            ..StartOptions::default()
        },
        "dic_1".into(),
        timing(),
    );
    stream.send_chunk("AAAA".into());
    let (mut socket, authorization) = sockets.recv().await.expect("a connection");
    assert_eq!(authorization.as_deref(), Some("Bearer oc_client_fixture"));
    send(&mut socket, json!({"type": "ready"})).await;
    let start = next(&mut socket).await;
    assert_eq!(start["type"], "start");
    assert_eq!(start["dictationId"], "dic_1");
    assert_eq!(start["format"], "audio/pcm;rate=16000;bits=16");
    assert_eq!(
        start["options"],
        json!({"provider": "local", "localModel": "parakeet-tdt-0.6b-v3-int8"})
    );
    send(
        &mut socket,
        json!({"type": "ack", "dictationId": "dic_1", "ackSeq": -1}),
    )
    .await;
    assert_eq!(event(&mut events).await, StreamEvent::Started);
    assert_eq!(
        next(&mut socket).await,
        json!({"type": "chunk", "dictationId": "dic_1", "seq": 0, "audio": "AAAA"}),
        "a chunk buffered before the ack goes first"
    );
    stream.send_chunk("BBBB".into());
    assert_eq!(next(&mut socket).await["seq"], 1);
    stream.finish();
    assert_eq!(
        next(&mut socket).await,
        json!({"type": "finish", "dictationId": "dic_1", "finalSeq": 1})
    );
    send(
        &mut socket,
        json!({"type": "partial", "dictationId": "dic_1", "text": "hello"}),
    )
    .await;
    send(
        &mut socket,
        json!({"type": "finish_accepted", "dictationId": "dic_1", "timeoutMs": 500}),
    )
    .await;
    send(
        &mut socket,
        json!({"type": "final", "dictationId": "dic_1", "text": "hello world"}),
    )
    .await;
    assert_eq!(
        event(&mut events).await,
        StreamEvent::Partial("hello".into())
    );
    assert_eq!(
        event(&mut events).await,
        StreamEvent::Final("hello world".into())
    );
}

#[tokio::test]
async fn a_downloading_model_retries_the_start_and_resends_every_chunk() {
    let (endpoint, mut sockets) = server().await;
    let (stream, mut events) = DictationStream::open(
        client(endpoint),
        StartOptions::default(),
        "dic_2".into(),
        timing(),
    );
    stream.send_chunk("AAAA".into());
    let (mut first, _) = sockets.recv().await.expect("first connection");
    send(&mut first, json!({"type": "ready"})).await;
    assert_eq!(next(&mut first).await["type"], "start");
    send(
        &mut first,
        json!({
            "type": "error", "dictationId": "dic_2", "error": "Downloading",
            "retryable": true, "reasonCode": "model_download_in_progress",
        }),
    )
    .await;
    let StreamEvent::StartFailed(error) = event(&mut events).await else {
        panic!("the start fails first");
    };
    assert!(error.model_downloading());
    stream.send_chunk("BBBB".into());
    let (mut second, _) = sockets.recv().await.expect("the retry connects again");
    send(&mut second, json!({"type": "ready"})).await;
    assert_eq!(next(&mut second).await["type"], "start");
    send(&mut second, json!({"type": "ack", "dictationId": "dic_2"})).await;
    assert_eq!(event(&mut events).await, StreamEvent::Started);
    assert_eq!(next(&mut second).await["audio"], "AAAA");
    assert_eq!(next(&mut second).await["audio"], "BBBB");
    stream.finish();
    assert_eq!(next(&mut second).await["finalSeq"], 1);
    send(
        &mut second,
        json!({"type": "error", "dictationId": "dic_2", "error": "Transcription failed", "retryable": true}),
    )
    .await;
    let StreamEvent::Failed(error) = event(&mut events).await else {
        panic!("the server's failure ends the dictation");
    };
    assert_eq!(error.message, "Transcription failed");
}

#[tokio::test]
async fn a_dropped_stream_cancels_its_dictation() {
    let (endpoint, mut sockets) = server().await;
    let (stream, mut events) = DictationStream::open(
        client(endpoint),
        StartOptions::default(),
        "dic_3".into(),
        timing(),
    );
    let (mut socket, _) = sockets.recv().await.expect("a connection");
    send(&mut socket, json!({"type": "ready"})).await;
    assert_eq!(next(&mut socket).await["type"], "start");
    send(&mut socket, json!({"type": "ack", "dictationId": "dic_3"})).await;
    assert_eq!(event(&mut events).await, StreamEvent::Started);
    drop(stream);
    assert_eq!(
        next(&mut socket).await,
        json!({"type": "cancel", "dictationId": "dic_3"})
    );
}

#[tokio::test]
async fn a_refused_connection_fails_at_finish() {
    // Nothing listens on this port once the listener is dropped.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = Endpoint::parse(&format!(
        "http://{}",
        listener.local_addr().expect("address")
    ))
    .expect("endpoint");
    drop(listener);
    let (stream, mut events) = DictationStream::open(
        client(endpoint),
        StartOptions::default(),
        "dic_4".into(),
        timing(),
    );
    stream.send_chunk("AAAA".into());
    assert!(matches!(
        event(&mut events).await,
        StreamEvent::StartFailed(_)
    ));
    stream.finish();
    let StreamEvent::Failed(error) = event(&mut events).await else {
        panic!("finish retries the start once and fails");
    };
    assert_eq!(error.message, "Dictation connection failed");
}
