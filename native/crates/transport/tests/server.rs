//! The transport against a scripted local server: authentication, request
//! fidelity, failure classification, paging, cancellation and the event
//! stream's resume, reset, fragmentation and timeout behavior.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use openchamber_native_transport::client::{ClientDescription, ServerClient, SettingsChanges};
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::ServerEvent;
use openchamber_native_transport::prompt::{
    Delivery, PromptAdmission, PromptObservation, SendStage, TextPrompt,
};
use openchamber_native_transport::stream::{EventStream, StreamItem};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};

#[path = "session_creation/mod.rs"]
mod session_creation;

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

fn text_prompt() -> TextPrompt {
    use openchamber_native_core::ids::MessageId;
    use openchamber_native_core::send_config::ModelChoice;
    TextPrompt {
        message_id: MessageId::new("msg_native_fixture_exact"),
        text: "Hello\n世界 ".to_owned(),
        model: Some(ModelChoice {
            provider_id: "fixture".to_owned(),
            model_id: "text-model".to_owned(),
            variant: Some("high".to_owned()),
        }),
        agent: Some("build".to_owned()),
        delivery: Some(Delivery::Queue),
    }
}

#[tokio::test]
async fn lookup_prompt_observes_pending_then_history_without_mutation() {
    use openchamber_native_core::ids::{MessageId, SessionId};
    let session = SessionId::new("ses_e9b49c443ffefXcWWPhCOga6nH");
    let message = MessageId::new("msg_native_fixture_exact");
    let inbox: Value =
        serde_json::from_str(include_str!("fixtures/wire/prompt-inbox.json")).expect("inbox");
    let history: Value =
        serde_json::from_str(include_str!("fixtures/wire/prompt-history.json")).expect("history");
    for pending in [true, false] {
        let inbox = inbox.clone();
        let history = history.clone();
        let server = Server::start(
            move |request| {
                if request.target.ends_with("/inbox") {
                    Reply::json(if pending {
                        inbox.clone()
                    } else {
                        json!({"data": []})
                    })
                } else {
                    Reply::json(history.clone())
                }
            },
            vec![],
        )
        .await;
        let client = ServerClient::new(
            server.endpoint(),
            Credential::ClientToken("lookup-secret".to_owned()),
        )
        .expect("client");
        let observed = client
            .lookup_prompt(&session, &message)
            .await
            .expect("observation");
        match observed {
            PromptObservation::Pending(receipt) => {
                assert!(pending);
                assert_eq!(receipt.session_id, session);
                assert_eq!(receipt.message_id, message);
                assert_eq!(receipt.delivery, Delivery::Queue);
                assert_eq!(receipt.created, 1792691938000);
            }
            PromptObservation::Delivered(record) => {
                assert!(!pending);
                assert_eq!(record.info.id, message);
                assert_eq!(record.plain_text(), "Prepared by the server");
            }
            PromptObservation::NotObserved => panic!("fixture establishes admission"),
        }
        let requests = server.requests();
        assert_eq!(requests.len(), if pending { 1 } else { 2 });
        assert_eq!(
            requests[0].target,
            format!("/prefix/api/session/{}/inbox", session.as_str())
        );
        if !pending {
            assert_eq!(
                requests[1].target,
                format!(
                    "/prefix/api/session/{}/message/{}",
                    session.as_str(),
                    message.as_str()
                )
            );
        }
        for request in requests {
            assert_eq!(request.method, "GET");
            assert!(request.body.is_empty());
            assert!(request.header("x-opencode-directory").is_none());
            assert!(request.header("x-openchamber-part-shells").is_none());
            assert_eq!(
                request.header("authorization"),
                Some("Bearer lookup-secret")
            );
        }
    }
}

#[tokio::test]
async fn lookup_prompt_absence_and_failures_remain_distinct() {
    use openchamber_native_core::ids::{MessageId, SessionId};
    for (inbox_status, history_status, expected) in [
        (200, 404, "absent"),
        (404, 200, "http"),
        (500, 200, "http"),
        (200, 500, "http"),
        (401, 200, "auth"),
        (200, 403, "auth"),
        (307, 200, "redirect"),
        (200, 307, "redirect"),
        (200, 200, "malformed"),
    ] {
        let server = Server::start(
            move |request| {
                let inbox = request.target.ends_with("/inbox");
                Reply::with_status(
                    if inbox { inbox_status } else { history_status },
                    "application/json",
                    if inbox { r#"{"data":[]}"# } else { "{}" },
                )
            },
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let result = client
            .lookup_prompt(&SessionId::new("ses_named"), &MessageId::new("msg_exact"))
            .await;
        match expected {
            "absent" => assert_eq!(
                result.expect("read completed"),
                PromptObservation::NotObserved
            ),
            "auth" => assert!(matches!(
                result,
                Err(TransportError::AuthenticationRequired)
            )),
            "redirect" => assert!(matches!(result, Err(TransportError::Redirected))),
            "http" => assert!(matches!(result, Err(TransportError::Http { .. }))),
            _ => assert!(matches!(result, Err(TransportError::Malformed(_)))),
        }
        assert_eq!(
            server.requests().len(),
            if inbox_status == 200 { 2 } else { 1 }
        );
    }
}

#[tokio::test]
async fn lookup_prompt_rejects_malformed_duplicate_and_conflicting_identities() {
    use openchamber_native_core::ids::{MessageId, SessionId};
    let valid = json!({"id":"msg_exact", "sessionID":"ses_named", "type":"user", "time":{"created":1}, "delivery":"queue"});
    let mut cases = vec![
        json!({}),
        json!({"data":null}),
        json!({"data":[valid.clone(), valid.clone()]}),
    ];
    for (key, value) in [
        ("id", json!(null)),
        ("sessionID", json!("ses_other")),
        ("type", json!("synthetic")),
        ("type", json!("future")),
        ("time", json!(null)),
        ("time", json!({"created":-1})),
        ("delivery", json!(null)),
        ("delivery", json!("secret-invalid-value")),
    ] {
        let mut item = valid.clone();
        item[key] = value;
        cases.push(json!({"data":[item]}));
    }
    for body in cases {
        let server = Server::start(move |_| Reply::json(body.clone()), vec![]).await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let error = client
            .lookup_prompt(&SessionId::new("ses_named"), &MessageId::new("msg_exact"))
            .await
            .expect_err("invalid inbox");
        assert!(matches!(error, TransportError::Malformed(_)));
        assert!(!format!("{error:?}").contains("secret-invalid-value"));
        assert_eq!(
            server.requests().len(),
            1,
            "invalid inbox never becomes absence"
        );
    }
    for (id, kind) in [("msg_other", "user"), ("msg_exact", "assistant")] {
        let server = Server::start(move |request| {
            Reply::json(if request.target.ends_with("/inbox") {
                json!({"data":[{"id":"msg_unrelated", "sessionID":"ses_named", "type":"compaction"}]})
            } else {
                json!({"data":{"id":id,"type":kind,"time":{"created":1},"text":"prepared"}})
            })
        }, vec![]).await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        assert!(matches!(
            client
                .lookup_prompt(&SessionId::new("ses_named"), &MessageId::new("msg_exact"))
                .await,
            Err(TransportError::Malformed(_))
        ));
        assert_eq!(server.requests().len(), 2);
    }
}

#[tokio::test]
async fn lookup_prompt_disconnect_is_an_error_without_retry() {
    use openchamber_native_core::ids::{MessageId, SessionId};
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("connection");
        let request = read_request(&mut socket, &mut Vec::new())
            .await
            .expect("request");
        drop(socket);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err(),
            "no retry after disconnect"
        );
        request
    });
    let client = ServerClient::new(
        Endpoint::parse(&format!("http://{address}")).expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    assert!(
        client
            .lookup_prompt(&SessionId::new("ses_named"), &MessageId::new("msg_exact"))
            .await
            .is_err()
    );
    let request = server.await.expect("server");
    assert_eq!(request.method, "GET");
    assert!(request.target.ends_with("/inbox"));
}

#[tokio::test]
async fn lookup_prompt_rejects_missing_user_text_and_invalid_input() {
    use openchamber_native_core::ids::{MessageId, SessionId};
    let server = Server::start(
        |request| {
            Reply::json(if request.target.ends_with("/inbox") {
                json!({"data":[]})
            } else {
                json!({"data":{"id":"msg_exact","type":"user","time":{"created":1}}})
            })
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    assert!(
        client
            .lookup_prompt(&SessionId::new(""), &MessageId::new("msg_exact"))
            .await
            .is_err()
    );
    assert!(
        client
            .lookup_prompt(&SessionId::new("ses_named"), &MessageId::new("invalid"))
            .await
            .is_err()
    );
    assert!(
        server.requests().is_empty(),
        "local validation sends no request"
    );
    assert!(matches!(
        client
            .lookup_prompt(&SessionId::new("ses_named"), &MessageId::new("msg_exact"))
            .await,
        Err(TransportError::Malformed(_))
    ));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn abort_failures_do_not_retry_and_keep_named_session() {
    use openchamber_native_core::ids::SessionId;
    for (status, body, unknown) in [
        (401, "{}", false),
        (307, "{}", true),
        (500, "{}", true),
        (200, "{}", true),
    ] {
        let server = Server::start(
            move |_| Reply::with_status(status, "application/json", body),
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let session = SessionId::new("ses_named");
        let error = client
            .abort_session(&session)
            .await
            .expect_err("failed interrupt");
        assert_eq!(error.session_id, session);
        assert_eq!(error.outcome_unknown, unknown);
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].target,
            "/prefix/api/session/ses_named/interrupt"
        );
    }
}

#[tokio::test]
async fn invalid_text_prompt_never_changes_session_selection() {
    use openchamber_native_core::ids::SessionId;
    let server = Server::start(|_| Reply::json(json!({})), vec![]).await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    let prompt = TextPrompt {
        text: " \n ".to_owned(),
        ..text_prompt()
    };
    let error = client
        .send_text(&SessionId::new("ses_named"), &prompt)
        .await
        .expect_err("blank prompt");
    assert_eq!(error.admission, PromptAdmission::NotAttempted);
    assert_eq!(error.stage, SendStage::Validation);
    assert_eq!(error.message_id, prompt.message_id);
    let invalid_id = TextPrompt {
        message_id: openchamber_native_core::ids::MessageId::new("invalid"),
        ..text_prompt()
    };
    let error = client
        .send_text(&SessionId::new("ses_named"), &invalid_id)
        .await
        .expect_err("invalid ID");
    assert_eq!(error.admission, PromptAdmission::NotAttempted);
    assert_eq!(error.stage, SendStage::Validation);
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn prompt_admission_selects_in_order_and_abort_names_only_that_session() {
    use openchamber_native_core::ids::SessionId;
    let server = Server::start(
        |request| {
            if request.target.ends_with("/prompt") {
                Reply::with_status(
                    200,
                    "application/json",
                    include_str!("fixtures/wire/prompt-admitted.json"),
                )
            } else if request.target.ends_with("/interrupt") {
                Reply::json(json!({"interrupted": true}))
            } else {
                Reply::with_status(204, "application/json", "")
            }
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("oc_client_test".to_owned()),
    )
    .expect("client");
    let session = SessionId::new("ses_e9b49c443ffefXcWWPhCOga6nH");
    let prompt = text_prompt();
    let receipt = client.send_text(&session, &prompt).await.expect("admitted");
    assert_eq!(
        receipt.message_id, prompt.message_id,
        "client ID is preserved exactly"
    );
    assert_eq!(receipt.session_id, session);
    assert_eq!(receipt.delivery, Delivery::Queue);
    assert!(client.abort_session(&session).await.expect("interrupt"));
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    for (request, operation) in requests
        .iter()
        .zip(["model", "agent", "prompt", "interrupt"])
    {
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            format!("/prefix/api/session/{}/{operation}", session.as_str())
        );
        assert_eq!(
            request.header("authorization"),
            Some("Bearer oc_client_test")
        );
        assert_eq!(request.header("x-opencode-directory"), None);
        assert_eq!(request.header("x-openchamber-part-shells"), None);
    }
    assert_eq!(
        requests[0].json(),
        json!({"model":{"providerID":"fixture","id":"text-model","variant":"high"}})
    );
    assert_eq!(requests[1].json(), json!({"agent":"build"}));
    assert_eq!(
        requests[2].json(),
        json!({"id":"msg_native_fixture_exact","text":"Hello\n世界 ","delivery":"queue"})
    );
    assert!(requests[3].body.is_empty());
}

#[tokio::test]
async fn prompt_admission_accepts_prepared_content_and_metadata() {
    use openchamber_native_core::ids::SessionId;
    for payload in [
        json!({"text":"Hello\n世界 ","metadata":{"source":"plugin"}}),
        json!({
            "text":"Prepared text from the session.prompt hook",
            "metadata":{"source":"plugin"},
            "files":[{"data":"aGk=","mime":"text/plain","source":{"type":"inline"},"name":"context.txt"}],
            "agents":[{"name":"plan"}],
            "skills":[{"id":"skill_fixture","name":"fixture","text":"Prepared skill context"}]
        }),
    ] {
        let server = Server::start(
            move |_| {
                Reply::json(json!({"data":{
                    "id":"msg_native_fixture_exact", "sessionID":"ses_named", "type":"user",
                    "time":{"created":1800000000000_u64}, "delivery":"steer", "payload":payload
                }}))
            },
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let session = SessionId::new("ses_named");
        let prompt = TextPrompt {
            model: None,
            agent: None,
            ..text_prompt()
        };
        let receipt = client
            .send_text(&session, &prompt)
            .await
            .expect("prepared user item is admitted");
        assert_eq!(receipt.session_id, session);
        assert_eq!(receipt.message_id, prompt.message_id);
        assert_eq!(receipt.created, 1800000000000);
        assert_eq!(
            receipt.delivery,
            Delivery::Steer,
            "preparation may change delivery too"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1, "admission needs no retry");
        assert_eq!(
            requests[0].json(),
            json!({
                "id":"msg_native_fixture_exact", "text":"Hello\n世界 ", "delivery":"queue"
            }),
            "request stays text-only and preserves the caller's text"
        );
    }
}

#[tokio::test]
async fn prompt_failures_preserve_correlation_and_never_retry() {
    use openchamber_native_core::ids::SessionId;
    for (status, body, admission) in [
        (400, "{}", PromptAdmission::Rejected),
        (401, "{}", PromptAdmission::Rejected),
        (403, "{}", PromptAdmission::Rejected),
        (404, "{}", PromptAdmission::Rejected),
        (409, "{}", PromptAdmission::Unknown),
        (500, "{}", PromptAdmission::Unknown),
        (200, "not json", PromptAdmission::Unknown),
        (204, "", PromptAdmission::Unknown),
        (
            200,
            r#"{"data":{"id":"wrong","sessionID":"s","time":{"created":1},"type":"user","payload":{"text":"Hello"},"delivery":"queue"}}"#,
            PromptAdmission::Unknown,
        ),
        (
            200,
            r#"{"data":{"id":"msg_native_fixture_exact","sessionID":"wrong","time":{"created":1},"type":"user","payload":{"text":"Hello"},"delivery":"queue"}}"#,
            PromptAdmission::Unknown,
        ),
        (
            200,
            r#"{"data":{"id":"msg_native_fixture_exact","sessionID":"s","time":{"created":1},"type":"synthetic","payload":{"text":"Hello"},"delivery":"queue"}}"#,
            PromptAdmission::Unknown,
        ),
    ] {
        let server = Server::start(
            move |_| Reply::with_status(status, "application/json", body),
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let prompt = TextPrompt {
            model: None,
            agent: None,
            ..text_prompt()
        };
        let session = SessionId::new("s");
        let error = client
            .send_text(&session, &prompt)
            .await
            .expect_err("failed receipt");
        assert_eq!(error.stage, SendStage::Prompt);
        assert_eq!(error.admission, admission, "HTTP {status}");
        assert_eq!(error.message_id, prompt.message_id);
        assert_eq!(error.session_id, session);
        assert_eq!(server.requests().len(), 1, "no mutation retry");
    }
}

#[tokio::test]
async fn selection_failure_stops_before_prompt_and_keeps_prior_selection() {
    use openchamber_native_core::ids::SessionId;
    for (failed_operation, stage, count) in [
        ("model", SendStage::Model, 1),
        ("agent", SendStage::Agent, 2),
    ] {
        let server = Server::start(
            move |request| {
                if request.target.ends_with(failed_operation) {
                    Reply::with_status(500, "application/json", "{}")
                } else {
                    Reply::with_status(204, "application/json", "")
                }
            },
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let error = client
            .send_text(&SessionId::new("s"), &text_prompt())
            .await
            .expect_err("selection failed");
        assert_eq!(error.stage, stage);
        assert_eq!(error.admission, PromptAdmission::NotAttempted);
        assert_eq!(
            server.requests().len(),
            count,
            "no rollback or prompt follows failure"
        );
    }
}

#[tokio::test]
async fn prompt_redirect_is_not_followed_and_abort_idle_is_authoritative() {
    use openchamber_native_core::ids::SessionId;
    let target = Server::start(|_| Reply::json(json!({})), vec![]).await;
    let redirect = format!("http://{}/stolen", target.address);
    let server = Server::start(
        move |request| {
            if request.target.ends_with("/interrupt") {
                return Reply::json(json!({"interrupted":false}));
            }
            let mut reply = Reply::with_status(307, "application/json", "{}");
            reply.headers.push(("location", redirect.clone()));
            reply
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("oc_client_test".to_owned()),
    )
    .expect("client");
    let prompt = TextPrompt {
        model: None,
        agent: None,
        ..text_prompt()
    };
    let error = client
        .send_text(&SessionId::new("s"), &prompt)
        .await
        .expect_err("redirect refused");
    assert!(matches!(error.source, TransportError::Redirected));
    assert_eq!(error.admission, PromptAdmission::Unknown);
    assert!(
        target.requests().is_empty(),
        "credentials cannot follow redirects"
    );
    assert!(
        !client
            .abort_session(&SessionId::new("s"))
            .await
            .expect("idle answer")
    );
}

#[tokio::test]
async fn lost_prompt_response_is_ambiguous_and_is_not_retried() {
    use openchamber_native_core::ids::SessionId;
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("connection");
        let request = read_request(&mut socket, &mut Vec::new())
            .await
            .expect("request before disconnect");
        drop(socket);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err(),
            "no retry after disconnect"
        );
        request
    });
    let client = ServerClient::new(
        Endpoint::parse(&format!("http://{address}")).expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    let prompt = TextPrompt {
        model: None,
        agent: None,
        ..text_prompt()
    };
    let error = client
        .send_text(&SessionId::new("s"), &prompt)
        .await
        .expect_err("lost response");
    assert_eq!(error.admission, PromptAdmission::Unknown);
    assert_eq!(error.message_id, prompt.message_id);
    assert_eq!(
        server.await.expect("server").json()["id"],
        "msg_native_fixture_exact"
    );
}

#[tokio::test]
async fn cancelling_prompt_io_does_not_interrupt_or_retry_server_work() {
    use openchamber_native_core::ids::SessionId;
    let server = Server::start(
        |_| {
            let mut reply = Reply::json(json!({}));
            reply.delay = Duration::from_secs(5);
            reply
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    let prompt = TextPrompt {
        model: None,
        agent: None,
        ..text_prompt()
    };
    let session = SessionId::new("s");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            client.send_text(&session, &prompt)
        )
        .await
        .is_err()
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].target.ends_with("/prompt"));
    assert_eq!(requests[0].json()["id"], prompt.message_id.as_str());
}

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
async fn older_messages_use_the_cursor_without_an_order_or_directory() {
    let server = Server::start(
        |request| {
            assert_eq!(request.method, "GET");
            assert_eq!(
                request.target,
                "/prefix/api/session/s/message?limit=10&cursor=%2B%2F%3D"
            );
            assert_eq!(request.header("x-opencode-directory"), None);
            Reply::json(json!({"data": [], "cursor": {"next": "ignored"}}))
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("a client");
    let page = client
        .earlier_messages(&"s".into(), "+/=", 10)
        .await
        .expect("older page succeeds");
    assert!(page.messages.is_empty());
    assert_eq!(
        page.older, None,
        "an empty page ends pagination even with a cursor"
    );
}

#[tokio::test]
async fn settings_writes_preserve_the_prefix_surface_and_unrelated_fields() {
    let server = Server::start(
        |request| {
            assert_eq!(
                request.target,
                "/prefix/api/config/settings?surface=desktop"
            );
            assert_eq!(request.method, "PUT");
            assert_eq!(request.json(), json!({"codeBlockLineWrap": false}));
            Reply::json(json!({"codeBlockLineWrap": false, "defaultAgent": "build"}))
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("fixture-settings-token".to_owned()),
    )
    .expect("a client");
    let settings = client
        .save_settings(&SettingsChanges {
            code_block_line_wrap: Some(false),
        })
        .await
        .expect("settings are saved");
    assert_eq!(settings.code_block_line_wrap, Some(false));
    assert_eq!(settings.default_agent.as_deref(), Some("build"));
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer fixture-settings-token")
    );
}

#[tokio::test]
async fn settings_write_failures_and_missing_values_remain_distinct() {
    let server = Server::start(
        |request| match request.json()["codeBlockLineWrap"].as_bool() {
            Some(false) => Reply::with_status(503, "application/json", "{}"),
            _ => Reply::json(json!({})),
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("a client");
    assert!(
        client
            .save_settings(&SettingsChanges {
                code_block_line_wrap: Some(false),
            })
            .await
            .is_err(),
        "a failed save cannot become empty success"
    );
    let settings = client
        .save_settings(&SettingsChanges {
            code_block_line_wrap: Some(true),
        })
        .await
        .expect("an older server may omit the field");
    assert_eq!(settings.code_block_line_wrap, None);
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
