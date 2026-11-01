use super::{Credential, Reply, Server, ServerClient, TransportError, json};

#[tokio::test]
async fn versions_use_authenticated_global_routes_and_preserve_independent_failures() {
    let server = Server::start(
        |request| {
            assert_eq!(request.method, "GET");
            assert_eq!(
                request.header("authorization"),
                Some("Bearer oc_client_fixture")
            );
            assert_eq!(request.header("x-opencode-directory"), None);
            match request.target.as_str() {
                "/prefix/api/system/info" => Reply::with_status(503, "application/json", "{}"),
                "/prefix/api/opencode/upgrade-status" => {
                    Reply::json(json!({"currentVersion":"2.2.0"}))
                }
                target => panic!("unexpected version route {target}"),
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
    let versions = client.system_versions().await;
    assert!(matches!(
        versions.openchamber,
        Err(TransportError::Http { status: 503 })
    ));
    assert_eq!(
        versions
            .opencode
            .expect("engine version survives app failure"),
        "2.2.0"
    );
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn malformed_version_labels_remain_unavailable() {
    let server = Server::start(
        |request| match request.target.as_str() {
            "/prefix/api/system/info" => Reply::json(json!({"openchamberVersion":"2.5.3"})),
            _ => Reply::json(json!({"currentVersion":""})),
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    let versions = client.system_versions().await;
    assert_eq!(versions.openchamber.expect("app version"), "2.5.3");
    assert!(versions.opencode.is_err());
}
