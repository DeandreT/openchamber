//! Multi-step prompts, synthetic context and slash commands against the
//! scripted server: request order, resumption, observation and error classes.

use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_transport::prompt::{CommandError, SendError, SendObservation};
use openchamber_native_transport::prompt_parts::{Command, PromptFile, SyntheticContext};

use super::*;

const SESSION: &str = "ses_e9b49c443ffefXcWWPhCOga6nH";

fn context(id: &str) -> SyntheticContext {
    SyntheticContext {
        id: MessageId::new(id),
        text: format!("context {id}"),
        description: Some("Selection".to_owned()),
        metadata: None,
    }
}

fn steered() -> Prompt {
    Prompt {
        parts: PromptParts {
            context: vec![context("msg_ctx_a"), context("msg_ctx_b")],
            ..PromptParts::default()
        },
        delivery: Some(Delivery::Steer),
        ..text_prompt()
    }
}

fn admitted(id: &str, kind: &str) -> Reply {
    Reply::json(json!({"data": {
        "id": id, "sessionID": SESSION, "type": kind,
        "time": {"created": 1}, "delivery": "steer", "payload": {}
    }}))
}

/// Answers selections with 204 and admissions with a matching receipt,
/// except the operations `fail` names.
fn scripted(
    fail: impl Fn(&Recorded) -> Option<Reply> + Send + Sync + 'static,
) -> impl Fn(&Recorded) -> Reply + Send + Sync + 'static {
    move |request| {
        if let Some(reply) = fail(request) {
            return reply;
        }
        let path = request.target.as_str();
        if path.ends_with("/model") || path.ends_with("/agent") || path.ends_with("/command") {
            return Reply::with_status(204, "application/json", "");
        }
        let id = request.json()["id"].as_str().unwrap_or_default().to_owned();
        if path.ends_with("/synthetic") {
            admitted(&id, "synthetic")
        } else {
            admitted(&id, "user")
        }
    }
}

fn operations(server: &Server) -> Vec<String> {
    server
        .requests()
        .iter()
        .map(|request| {
            let operation = request.target.rsplit('/').next().unwrap_or_default();
            match request.json().get("id").and_then(Value::as_str) {
                Some(id) => format!("{operation}:{id}"),
                None => operation.to_owned(),
            }
        })
        .collect()
}

async fn send(server: &Server, prompt: &Prompt, skip: &[MessageId]) -> Result<(), SendError> {
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    client
        .resume_prompt(&SessionId::new(SESSION), prompt, skip)
        .await
        .map(|_| ())
}

#[tokio::test]
async fn context_is_admitted_in_order_before_the_prompt_with_its_delivery() {
    let server = Server::start(scripted(|_| None), vec![]).await;
    send(&server, &steered(), &[]).await.expect("admitted");
    assert_eq!(
        operations(&server),
        [
            "model",
            "agent",
            "synthetic:msg_ctx_a",
            "synthetic:msg_ctx_b",
            "prompt:msg_native_fixture_exact"
        ]
    );
    let requests = server.requests();
    for synthetic in &requests[2..4] {
        let body = synthetic.json();
        assert_eq!(
            body["delivery"], "steer",
            "context shares the prompt's delivery"
        );
        assert_eq!(body["resume"], false, "context never starts a turn");
        assert_eq!(body["description"], "Selection");
    }
    assert_eq!(requests[4].json()["delivery"], "steer");
    assert!(
        requests[4].json().get("files").is_none(),
        "empty parts are omitted from the prompt body"
    );
}

#[tokio::test]
async fn resume_sends_only_the_missing_steps_in_order() {
    let server = Server::start(scripted(|_| None), vec![]).await;
    send(&server, &steered(), &[MessageId::new("msg_ctx_a")])
        .await
        .expect("resumed");
    assert_eq!(
        operations(&server),
        [
            "model",
            "agent",
            "synthetic:msg_ctx_b",
            "prompt:msg_native_fixture_exact"
        ]
    );
    let server = Server::start(scripted(|_| None), vec![]).await;
    send(
        &server,
        &steered(),
        &[MessageId::new("msg_ctx_a"), MessageId::new("msg_ctx_b")],
    )
    .await
    .expect("resumed");
    assert_eq!(
        operations(&server),
        ["model", "agent", "prompt:msg_native_fixture_exact"]
    );
}

#[tokio::test]
async fn invalid_parts_and_out_of_order_resumption_send_nothing() {
    let server = Server::start(scripted(|_| None), vec![]).await;
    let mut duplicate = steered();
    duplicate.parts.context[1].id = MessageId::new("msg_ctx_a");
    let mut blank_file = text_prompt();
    blank_file.parts.files.push(PromptFile {
        uri: " ".to_owned(),
        name: None,
    });
    for (prompt, skip) in [
        (steered(), vec![MessageId::new("msg_ctx_b")]),
        (
            steered(),
            vec![
                MessageId::new("msg_ctx_a"),
                MessageId::new("msg_ctx_b"),
                MessageId::new("msg_ctx_c"),
            ],
        ),
        (duplicate, Vec::new()),
        (blank_file, Vec::new()),
    ] {
        let error = send(&server, &prompt, &skip).await.expect_err("invalid");
        assert_eq!(error.stage, SendStage::Validation);
        assert_eq!(error.admission, PromptAdmission::NotAttempted);
    }
    assert!(server.requests().is_empty());
    // A prompt with only context or a file and no text is valid, as in the web client.
    let server = Server::start(scripted(|_| None), vec![]).await;
    let context_only = Prompt {
        text: String::new(),
        ..steered()
    };
    send(&server, &context_only, &[])
        .await
        .expect("context-only prompt");
}

#[tokio::test]
async fn context_failures_keep_the_failed_index_and_admission_certainty() {
    for (status, expected) in [
        // The first item is admitted, so a refusal of the second is partial.
        (400, PromptAdmission::Partial),
        (404, PromptAdmission::Partial),
        (409, PromptAdmission::Unknown),
        (500, PromptAdmission::Unknown),
    ] {
        let server = Server::start(
            scripted(move |request| {
                (request.target.ends_with("/synthetic") && request.json()["id"] == "msg_ctx_b")
                    .then(|| Reply::with_status(status, "application/json", "{}"))
            }),
            vec![],
        )
        .await;
        let error = send(&server, &steered(), &[]).await.expect_err("failed");
        assert_eq!(error.stage, SendStage::Context { index: 1 }, "{status}");
        assert_eq!(error.admission, expected, "{status}");
        assert_eq!(error.message_id.as_str(), "msg_native_fixture_exact");
        assert!(
            !operations(&server)
                .iter()
                .any(|op| op.starts_with("prompt")),
            "the prompt never follows a failed context step"
        );
    }
    // A receipt for another item cannot count as this context's admission.
    let server = Server::start(
        scripted(|request| {
            request
                .target
                .ends_with("/synthetic")
                .then(|| admitted("msg_ctx_a", "user"))
        }),
        vec![],
    )
    .await;
    let error = send(&server, &steered(), &[]).await.expect_err("mismatch");
    assert_eq!(error.stage, SendStage::Context { index: 0 });
    assert_eq!(error.admission, PromptAdmission::Unknown);
}

#[tokio::test]
async fn prompt_status_codes_map_to_declared_outcomes() {
    for (status, expected) in [
        (400, PromptAdmission::Rejected),
        (404, PromptAdmission::Rejected),
        (409, PromptAdmission::Unknown),
        (500, PromptAdmission::Unknown),
    ] {
        let server = Server::start(
            scripted(move |request| {
                request
                    .target
                    .ends_with("/prompt")
                    .then(|| Reply::with_status(status, "application/json", "{}"))
            }),
            vec![],
        )
        .await;
        let error = send(&server, &text_prompt(), &[])
            .await
            .expect_err("failed");
        assert_eq!(error.stage, SendStage::Prompt, "{status}");
        assert_eq!(error.admission, expected, "{status}");
    }
}

fn command() -> Command {
    Command {
        name: "review".to_owned(),
        text: "src/lib.rs".to_owned(),
        files: Vec::new(),
        context: vec![context("msg_ctx_a")],
        model: None,
        agent: Some("build".to_owned()),
        delivery: Some(Delivery::Queue),
    }
}

async fn run(server: &Server, command: &Command) -> Result<(), CommandError> {
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    client.run_command(&SessionId::new(SESSION), command).await
}

#[tokio::test]
async fn command_runs_after_selection_and_context_and_reports_no_identity() {
    let server = Server::start(scripted(|_| None), vec![]).await;
    run(&server, &command()).await.expect("204");
    assert_eq!(
        operations(&server),
        ["agent", "synthetic:msg_ctx_a", "command"]
    );
    let body = server.requests()[2].json();
    assert_eq!(
        body,
        json!({"name": "review", "text": "src/lib.rs", "delivery": "queue"})
    );
    for (status, expected) in [
        // Its context item is already admitted.
        (400, PromptAdmission::Partial),
        (404, PromptAdmission::Partial),
        // Declared, but the command may have started before failing.
        (500, PromptAdmission::Unknown),
        // An undeclared success-like status is not the declared 204.
        (200, PromptAdmission::Unknown),
    ] {
        let server = Server::start(
            scripted(move |request| {
                request
                    .target
                    .ends_with("/command")
                    .then(|| Reply::with_status(status, "application/json", "{}"))
            }),
            vec![],
        )
        .await;
        let error = run(&server, &command()).await.expect_err("failed");
        assert_eq!(error.stage, SendStage::Command, "{status}");
        assert_eq!(error.admission, expected, "{status}");
    }
    let server = Server::start(scripted(|_| None), vec![]).await;
    for name in ["", " ", "/review"] {
        let error = run(
            &server,
            &Command {
                name: name.to_owned(),
                ..command()
            },
        )
        .await
        .expect_err("invalid name");
        assert_eq!(error.stage, SendStage::Validation);
        assert_eq!(error.admission, PromptAdmission::NotAttempted);
    }
    assert!(server.requests().is_empty());
}

/// Inbox and history as an observation sees them.
fn observed(
    inbox: Vec<(&'static str, &'static str)>,
    history: Vec<(&'static str, &'static str)>,
) -> impl Fn(&Recorded) -> Reply + Send + Sync + 'static {
    move |request| {
        if request.target.ends_with("/inbox") {
            let items: Vec<Value> = inbox
                .iter()
                .map(|(id, kind)| {
                    json!({"id": id, "sessionID": SESSION, "type": kind,
                        "time": {"created": 1}, "delivery": "steer", "payload": {"text": "x"}})
                })
                .collect();
            return Reply::json(json!({ "data": items }));
        }
        let id = request.target.rsplit('/').next().unwrap_or_default();
        match history.iter().find(|(known, _)| *known == id) {
            Some((id, kind)) => Reply::json(json!({"data": {
                "id": id, "type": kind, "text": "x", "time": {"created": 1}
            }})),
            None => Reply::with_status(404, "application/json", "{}"),
        }
    }
}

async fn observe(server: &Server) -> Result<SendObservation, TransportError> {
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    client
        .observe_send(&SessionId::new(SESSION), &steered())
        .await
}

#[tokio::test]
async fn observation_reads_each_step_and_reports_the_admitted_prefix() {
    let server = Server::start(
        observed(vec![("msg_native_fixture_exact", "user")], vec![]),
        vec![],
    )
    .await;
    assert!(matches!(
        observe(&server).await.expect("read"),
        SendObservation::Pending(_)
    ));
    assert_eq!(
        server.requests().len(),
        1,
        "a pending prompt settles every step"
    );

    let server = Server::start(
        observed(
            vec![("msg_ctx_a", "synthetic")],
            vec![("msg_unrelated", "user")],
        ),
        vec![],
    )
    .await;
    assert_eq!(
        observe(&server).await.expect("read"),
        SendObservation::Missing {
            admitted: vec![MessageId::new("msg_ctx_a")]
        }
    );
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.method == "GET"),
        "observation never mutates"
    );

    let server = Server::start(observed(vec![], vec![("msg_ctx_b", "synthetic")]), vec![]).await;
    assert_eq!(
        observe(&server).await.expect("read"),
        SendObservation::OutOfOrder
    );

    let server = Server::start(
        observed(vec![], vec![("msg_native_fixture_exact", "user")]),
        vec![],
    )
    .await;
    assert!(matches!(
        observe(&server).await.expect("read"),
        SendObservation::Delivered(_)
    ));

    // A context ID answered with another kind is a conflict, never admission.
    for server in [
        Server::start(observed(vec![("msg_ctx_a", "user")], vec![]), vec![]).await,
        Server::start(observed(vec![], vec![("msg_ctx_a", "user")]), vec![]).await,
    ] {
        assert!(matches!(
            observe(&server).await,
            Err(TransportError::Malformed(_))
        ));
    }
}

#[tokio::test]
async fn a_refusal_after_admitted_context_is_partial_and_never_a_rollback() {
    use openchamber_native_core::ids::MessageId as Id;
    // (failing operation, status, skip already admitted, expected stage, admission)
    let cases: Vec<(&str, u16, Vec<Id>, SendStage, PromptAdmission)> = vec![
        (
            "prompt",
            400,
            vec![],
            SendStage::Prompt,
            PromptAdmission::Partial,
        ),
        (
            "prompt",
            401,
            vec![],
            SendStage::Prompt,
            PromptAdmission::Partial,
        ),
        (
            "msg_ctx_b",
            400,
            vec![],
            SendStage::Context { index: 1 },
            PromptAdmission::Partial,
        ),
        (
            "msg_ctx_b",
            403,
            vec![],
            SendStage::Context { index: 1 },
            PromptAdmission::Partial,
        ),
        (
            "model",
            400,
            vec![Id::new("msg_ctx_a")],
            SendStage::Model,
            PromptAdmission::Partial,
        ),
        (
            "msg_ctx_b",
            404,
            vec![Id::new("msg_ctx_a")],
            SendStage::Context { index: 1 },
            PromptAdmission::Partial,
        ),
        // Nothing was admitted yet: the earlier definite outcomes stay.
        (
            "msg_ctx_a",
            400,
            vec![],
            SendStage::Context { index: 0 },
            PromptAdmission::NotAttempted,
        ),
        (
            "model",
            400,
            vec![],
            SendStage::Model,
            PromptAdmission::NotAttempted,
        ),
        // A lost answer is still unknown.
        (
            "prompt",
            500,
            vec![],
            SendStage::Prompt,
            PromptAdmission::Unknown,
        ),
    ];
    for (operation, status, skip, stage, expected) in cases {
        let server = Server::start(
            scripted(move |request| {
                let hit = request.target.ends_with(&format!("/{operation}"))
                    || (request.target.ends_with("/synthetic")
                        && request.json()["id"] == operation);
                hit.then(|| Reply::with_status(status, "application/json", "{}"))
            }),
            vec![],
        )
        .await;
        let error = send(&server, &steered(), &skip).await.expect_err("failed");
        assert_eq!(
            (error.stage, error.admission),
            (stage, expected),
            "{operation} {status} after {} admitted",
            skip.len()
        );
    }
    let without_context = Prompt {
        parts: PromptParts::default(),
        ..steered()
    };
    let server = Server::start(
        scripted(|request| {
            request
                .target
                .ends_with("/prompt")
                .then(|| Reply::with_status(400, "application/json", "{}"))
        }),
        vec![],
    )
    .await;
    let error = send(&server, &without_context, &[])
        .await
        .expect_err("rejected");
    assert_eq!(error.admission, PromptAdmission::Rejected);
}

#[tokio::test]
async fn a_command_refused_after_its_context_is_partial() {
    for (operation, status, expected) in [
        ("command", 400, PromptAdmission::Partial),
        ("command", 401, PromptAdmission::Partial),
        ("synthetic", 400, PromptAdmission::NotAttempted),
    ] {
        let server = Server::start(
            scripted(move |request| {
                request
                    .target
                    .ends_with(&format!("/{operation}"))
                    .then(|| Reply::with_status(status, "application/json", "{}"))
            }),
            vec![],
        )
        .await;
        let error = run(&server, &command()).await.expect_err("failed");
        assert_eq!(error.admission, expected, "{operation} {status}");
    }
    let server = Server::start(
        scripted(|request| {
            request
                .target
                .ends_with("/command")
                .then(|| Reply::with_status(400, "application/json", "{}"))
        }),
        vec![],
    )
    .await;
    let error = run(
        &server,
        &Command {
            context: Vec::new(),
            ..command()
        },
    )
    .await
    .expect_err("rejected");
    assert_eq!(error.admission, PromptAdmission::Rejected);
}

#[tokio::test]
async fn only_a_declared_refusal_of_a_selection_is_definite() {
    for (status, definite) in [
        (400, true),
        (401, true),
        (403, true),
        (404, true),
        (409, false),
        (500, false),
    ] {
        let server = Server::start(
            scripted(move |request| {
                request
                    .target
                    .ends_with("/model")
                    .then(|| Reply::with_status(status, "application/json", "{}"))
            }),
            vec![],
        )
        .await;
        let error = send(&server, &steered(), &[]).await.expect_err("failed");
        assert_eq!(error.stage, SendStage::Model);
        assert_eq!(error.is_definite(), definite, "model {status}");
    }
}
