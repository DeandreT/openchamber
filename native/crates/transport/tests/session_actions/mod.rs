//! Session management against the scripted authenticated HTTP server.

use openchamber_native_core::ids::SessionId;
use openchamber_native_transport::session_actions::{Deleted, WorkState};

use super::*;

const SESSION: &str = "ses_e9b49c443ffefXcWWPhCOga6nH";

fn session_record(metadata: Value) -> Value {
    let mut sample: Value =
        serde_json::from_str(include_str!("../fixtures/wire/session-created.json"))
            .expect("sample");
    sample["data"]["metadata"] = metadata;
    sample
}

fn no_content() -> Reply {
    Reply::with_status(204, "application/json", "")
}

#[tokio::test]
async fn rename_reads_the_record_back_and_notes_a_user_title() {
    let server = Server::start(
        |request| match request.method.as_str() {
            "PATCH" => no_content(),
            "GET" => Reply::json(session_record(
                json!({ "openchamber": { "pinned": { "at": 3 } } }),
            )),
            _ => Reply::json(json!({ "metadata": {} })),
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("management-token".to_owned()),
    )
    .expect("client");
    let record = client
        .rename_session(&SessionId::new(SESSION), "Renamed")
        .await
        .expect("renamed");
    assert_eq!(record.id.as_str(), SESSION);
    assert_eq!(record.overlay.pinned_at, Some(3));
    let requests = server.requests();
    let methods: Vec<&str> = requests
        .iter()
        .map(|request| request.method.as_str())
        .collect();
    assert_eq!(methods, ["PATCH", "GET", "POST"]);
    assert_eq!(requests[0].target, format!("/prefix/api/session/{SESSION}"));
    assert_eq!(requests[0].json(), json!({ "title": "Renamed" }));
    assert_eq!(
        requests[2].target,
        format!("/prefix/api/openchamber/sessions/{SESSION}/metadata")
    );
    assert_eq!(
        requests[2].json(),
        json!({ "patch": { "openchamber": { "workTitle": { "title": "Renamed", "by": "user" } } } })
    );
    for request in &requests {
        assert_eq!(
            request.header("authorization"),
            Some("Bearer management-token")
        );
        assert!(request.header("x-opencode-directory").is_none());
    }
}

#[tokio::test]
async fn a_failed_rename_sends_nothing_more_and_keeps_its_status() {
    let server = Server::start(
        |_| Reply::with_status(500, "application/json", "{\"secret\":1}"),
        vec![],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    let error = client
        .rename_session(&SessionId::new(SESSION), "Renamed")
        .await
        .expect_err("server failure");
    assert!(matches!(error, TransportError::Http { status: 500 }));
    assert!(!error.to_string().contains("secret"));
    assert_eq!(server.requests().len(), 1, "no read back after a failure");
}

#[tokio::test]
async fn delete_reports_removal_absence_and_failure_distinctly() {
    for (status, expected) in [
        (204, Some(Deleted::Removed)),
        (404, Some(Deleted::AlreadyGone)),
        (500, None),
    ] {
        let server = Server::start(
            move |_| Reply::with_status(status, "application/json", ""),
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let outcome = client.delete_session(&SessionId::new(SESSION)).await;
        match expected {
            Some(expected) => assert_eq!(outcome.expect("settled"), expected),
            None => assert!(matches!(outcome, Err(TransportError::Http { status: 500 }))),
        }
        let requests = server.requests();
        assert_eq!(requests.len(), 1, "never retried");
        assert_eq!(requests[0].method, "DELETE");
    }
}

#[tokio::test]
async fn archive_and_restore_keep_partial_results_and_reject_malformed_answers() {
    let server = Server::start(
        |request| {
            if request.target.ends_with("/archive") {
                Reply::json(json!({ "archived": [{ "id": "ses_a", "archivedAt": 9 }], "failedIds": ["ses_b"] }))
            } else {
                Reply::json(json!({ "restored": "not a list" }))
            }
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    let ids = [SessionId::new("ses_a"), SessionId::new("ses_b")];
    let archived = client
        .archive_sessions("/work/app", &ids, 9)
        .await
        .expect("answered");
    assert_eq!(archived.archived, [(SessionId::new("ses_a"), 9)]);
    assert_eq!(archived.failed, [SessionId::new("ses_b")]);
    let error = client
        .restore_sessions(&ids[..1])
        .await
        .expect_err("a malformed answer is not an empty success");
    assert!(matches!(error, TransportError::Malformed(_)));
    let requests = server.requests();
    assert_eq!(
        requests[0].json(),
        json!({ "directory": "/work/app", "ids": ["ses_a", "ses_b"], "archivedAt": 9 })
    );
    assert_eq!(
        requests[1].target,
        "/prefix/api/openchamber/sessions/unarchive"
    );
}

#[tokio::test]
async fn pin_and_work_patch_from_a_fresh_read() {
    let server = Server::start(
        |request| match request.method.as_str() {
            "GET" => Reply::json(session_record(json!({ "openchamber": { "work": { "state": "open", "openedAt": 2 } }, "other": true }))),
            _ => Reply::json(json!({ "metadata": { "openchamber": { "pinned": { "at": 5 }, "work": { "state": "done" } } } })),
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    let session = SessionId::new(SESSION);
    let facts = client
        .set_session_pinned(&session, true, 5)
        .await
        .expect("pinned");
    assert_eq!(facts.overlay.pinned_at, Some(5));
    assert!(!facts.overlay.in_work, "the server's merged answer wins");
    client
        .set_session_work_state(&session, WorkState::Done, 8)
        .await
        .expect("done");
    let requests = server.requests();
    assert_eq!(
        requests[1].json(),
        json!({ "patch": { "openchamber": { "pinned": { "at": 5 } } } })
    );
    assert_eq!(
        requests[3].json(),
        json!({ "patch": { "openchamber": { "work": { "state": "done", "doneAt": 8 } } } })
    );
}
