//! Creation and observation against the scripted authenticated HTTP server.

use openchamber_native_core::ids::SessionId;
use openchamber_native_transport::session_create::{
    CreateSession, CreationOutcome, SessionError, SessionObservation,
};

use super::*;

fn creation() -> CreateSession {
    CreateSession::new(
        SessionId::new("ses_e9b49c443ffefXcWWPhCOga6nH"),
        "/home/synthetic/my project/ünï",
        None,
        None,
    )
    .expect("valid request")
}

fn sample() -> Value {
    serde_json::from_str(include_str!("../fixtures/wire/session-created.json")).expect("sample")
}

#[tokio::test]
async fn creation_and_exact_get_preserve_auth_prefix_and_saved_body() {
    let server = Server::start(|_| Reply::json(sample()), vec![]).await;
    let client = ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("creation-test-token".to_owned()),
    )
    .expect("client");
    let create = creation();
    let first = client.create_session(&create).await.expect("created");
    let second = client
        .create_session(&create)
        .await
        .expect("explicit same-ID retry");
    assert_eq!(first.id, second.id);
    assert!(matches!(
        client
            .lookup_session(create.id(), create.directory())
            .await
            .expect("read"),
        SessionObservation::Found(_)
    ));
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].target, "/prefix/api/session");
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].body, requests[1].body,
        "retry must preserve the body"
    );
    assert_eq!(requests[2].method, "GET");
    assert_eq!(
        requests[2].target,
        format!("/prefix/api/session/{}", create.id())
    );
    assert!(requests[2].body.is_empty());
    for request in requests {
        assert_eq!(
            request.header("authorization"),
            Some("Bearer creation-test-token")
        );
        assert!(request.header("x-opencode-directory").is_none());
    }
}

#[tokio::test]
async fn creation_mismatches_and_malformed_responses_never_mutate_again() {
    for case in ["id", "directory", "malformed"] {
        let server = Server::start(
            move |_| {
                let mut body = sample();
                match case {
                    "id" => body["data"]["id"] = "ses_unrelated".into(),
                    "directory" => body["data"]["location"]["directory"] = "/unrelated".into(),
                    _ => body["data"]["time"] = "sensitive invalid content".into(),
                }
                Reply::json(body)
            },
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let create = creation();
        let error = client
            .create_session(&create)
            .await
            .expect_err("unsafe receipt");
        assert_eq!(error.outcome, CreationOutcome::Unknown);
        match case {
            "id" => assert!(matches!(error.source, SessionError::IdentityMismatch)),
            "directory" => assert!(matches!(error.source, SessionError::LocationMismatch)),
            _ => assert!(matches!(error.source, SessionError::InvalidResponse)),
        }
        assert!(!format!("{error:?}").contains("sensitive invalid content"));
        assert!(
            client
                .lookup_session(create.id(), create.directory())
                .await
                .is_err()
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, "GET");
    }
}

#[tokio::test]
async fn creation_failure_classes_and_lookup_absence_remain_distinct() {
    for status in [201, 400, 401, 403, 404, 409, 500, 503] {
        let server = Server::start(
            move |_| Reply::with_status(status, "application/json", "{}"),
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let create = creation();
        let error = client
            .create_session(&create)
            .await
            .expect_err("POST failure");
        assert_eq!(
            error.outcome,
            if matches!(status, 400 | 401 | 403 | 404) {
                CreationOutcome::Rejected
            } else {
                CreationOutcome::Unknown
            }
        );
        let observed = client.lookup_session(create.id(), create.directory()).await;
        if status == 404 {
            assert!(matches!(observed, Ok(SessionObservation::NotObserved)));
        } else {
            assert!(observed.is_err());
        }
        assert_eq!(server.requests().len(), 2, "no automatic requests");
    }
}

#[tokio::test]
async fn creation_and_lookup_refuse_redirects_without_forwarding_credentials() {
    let target = Server::start(|_| Reply::json(sample()), vec![]).await;
    let location = format!("http://{}/stolen", target.address);
    let server = Server::start(
        move |_| {
            let mut reply = Reply::with_status(307, "application/json", "{}");
            reply.headers.push(("Location", location.clone()));
            reply
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("private-test-token".to_owned()),
    )
    .expect("client");
    let create = creation();
    let error = client.create_session(&create).await.expect_err("redirect");
    assert_eq!(error.outcome, CreationOutcome::Unknown);
    assert!(matches!(
        error.source,
        SessionError::Transport(TransportError::Redirected)
    ));
    assert!(matches!(
        client.lookup_session(create.id(), create.directory()).await,
        Err(SessionError::Transport(TransportError::Redirected))
    ));
    assert!(target.requests().is_empty());
}

#[tokio::test]
async fn lost_creation_response_is_unknown_without_automatic_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("connection");
        let request = read_request(&mut socket, &mut Vec::new())
            .await
            .expect("received POST");
        drop(socket);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err()
        );
        request
    });
    let client = ServerClient::new(
        Endpoint::parse(&format!("http://{address}")).expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    let create = creation();
    let error = client
        .create_session(&create)
        .await
        .expect_err("lost response");
    assert_eq!(error.outcome, CreationOutcome::Unknown);
    assert_eq!(error.session_id, *create.id());
    assert_eq!(
        server.await.expect("server").json()["id"],
        create.id().as_str()
    );
}

#[tokio::test]
async fn cancelling_creation_leaves_only_the_original_post() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback");
    let address = listener.local_addr().expect("address");
    let (received, receipt) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("connection");
        let request = read_request(&mut socket, &mut Vec::new())
            .await
            .expect("received POST");
        received.send(request).expect("waiting test");
        let mut byte = [0];
        let _ = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err()
        );
    });
    let client = ServerClient::new(
        Endpoint::parse(&format!("http://{address}")).expect("endpoint"),
        Credential::None,
    )
    .expect("client");
    let task = tokio::spawn(async move { client.create_session(&creation()).await });
    let request = receipt.await.expect("POST arrived before cancellation");
    task.abort();
    assert!(task.await.expect_err("cancelled").is_cancelled());
    assert_eq!(request.method, "POST");
    server.await.expect("server");
}
