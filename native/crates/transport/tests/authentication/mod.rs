use openchamber_native_transport::authentication::{AuthenticationStatus, LoginTrust};

use super::{Credential, Reply, Server, ServerClient, TransportError, description, json};

#[tokio::test]
async fn status_distinguishes_password_pairing_and_authenticated_clients() {
    for (status, body, expected) in [
        (
            200,
            json!({"authenticated":true}),
            AuthenticationStatus::Authenticated,
        ),
        (
            401,
            json!({"authenticated":false,"locked":true}),
            AuthenticationStatus::PasswordRequired,
        ),
        (
            401,
            json!({"authenticated":false,"locked":true,"clientAuthRequired":true}),
            AuthenticationStatus::ClientTokenRequired,
        ),
    ] {
        let server = Server::start(
            move |_| Reply::with_status(status, "application/json", body.to_string()),
            vec![],
        )
        .await;
        let client = ServerClient::new(
            server.endpoint(),
            Credential::ClientToken("oc_client_exact".into()),
        )
        .expect("client");
        assert_eq!(
            client.authentication_status().await.expect("valid status"),
            expected
        );
        let requests = server.requests();
        assert_eq!(requests[0].target, "/prefix/auth/session");
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests[0].header("authorization"),
            Some("Bearer oc_client_exact")
        );
    }
}

#[tokio::test]
async fn invalid_or_failed_status_never_authorizes_or_prompts_for_a_password() {
    for (status, content_type, body) in [
        (
            200,
            "application/json",
            r#"{"authenticated":false}"#.to_owned(),
        ),
        (
            401,
            "application/json",
            r#"{"authenticated":true}"#.to_owned(),
        ),
        (
            401,
            "application/json",
            r#"{"authenticated":false}"#.to_owned(),
        ),
        (
            401,
            "application/json",
            r#"{"authenticated":"false","locked":true}"#.to_owned(),
        ),
        (200, "text/html", "<html>proxy login</html>".to_owned()),
        (502, "application/json", "{}".to_owned()),
        (302, "application/json", "{}".to_owned()),
        (200, "application/json", "x".repeat(16 * 1024 + 1)),
    ] {
        let server = Server::start(
            move |_| Reply::with_status(status, content_type, &body),
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        assert!(
            client.authentication_status().await.is_err(),
            "HTTP {status} cannot grant authority"
        );
    }
}

#[tokio::test]
async fn password_login_preserves_the_explicit_trust_choice() {
    for (trust, expected) in [(LoginTrust::Session, false), (LoginTrust::Device, true)] {
        let server = Server::start(
            |_| Reply::json(json!({"authenticated":true,"clientToken":"oc_client_valid"})),
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        assert_eq!(
            client
                .login_with_trust("fixture password", &description(), trust)
                .await
                .expect("signed in"),
            Credential::ClientToken("oc_client_valid".into())
        );
        assert_eq!(server.requests()[0].json()["trustDevice"], expected);
    }
}

#[tokio::test]
async fn malformed_tokens_and_unauthenticated_login_answers_are_refused() {
    for body in [
        json!({"authenticated":false,"clientToken":"oc_client_valid"}),
        json!({"clientToken":"oc_client_valid"}),
        json!({"authenticated":true,"clientToken":"oc_client_"}),
        json!({"authenticated":true,"clientToken":"oc_client_bad\r\nheader"}),
        json!({"authenticated":true,"clientToken":format!("oc_client_{}", "x".repeat(4096))}),
    ] {
        let server = Server::start(move |_| Reply::json(body.clone()), vec![]).await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        assert!(
            client
                .login_with_trust("fixture", &description(), LoginTrust::Session)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn login_rate_limit_keeps_the_server_delay_and_never_retries() {
    let server = Server::start(
        |_| {
            let mut reply = Reply::with_status(429, "application/json", "{}");
            reply.headers.push(("Retry-After", "72".into()));
            reply
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    assert!(matches!(
        client
            .login_with_trust("fixture", &description(), LoginTrust::Session)
            .await,
        Err(TransportError::AuthenticationRateLimited {
            retry_after_seconds: Some(72)
        })
    ));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn all_http_401_responses_signal_the_same_client_without_treating_403_as_revocation() {
    let server = Server::start(
        |request| {
            if request.target.contains("/api/fs/read") {
                Reply::with_status(
                    403,
                    "application/json",
                    r#"{"error":"Path outside workspace","reason":"os-permission"}"#,
                )
            } else {
                Reply::with_status(
                    401,
                    "application/json",
                    r#"{"error":"Authentication required"}"#,
                )
            }
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(
        server.endpoint(),
        Credential::ClientToken("oc_client_fixture".into()),
    )
    .expect("client");
    let mut observed = client.authentication_failures();
    assert_eq!(*observed.borrow_and_update(), 0);
    assert!(client.settings().await.is_err());
    assert_eq!(*observed.borrow_and_update(), 1);
    assert!(client.clone().session_page(None).await.is_err());
    assert_eq!(*observed.borrow_and_update(), 2);
    assert!(
        client
            .read_file("/repo", "/repo/file.txt", false)
            .await
            .is_err()
    );
    assert_eq!(
        *observed.borrow_and_update(),
        2,
        "filesystem permission refusals do not revoke authentication"
    );
}
