use super::{Credential, Reply, Server, ServerClient, TransportError, json};
use openchamber_native_transport::projects::ProjectEntries;
use openchamber_native_transport::terminal::{TerminalApi, TerminalRequestError, TerminalSettings};

const SETTINGS: &str = r#"{
    "themeId":"A\ud800B","darkThemeId":"A\ud800B","\udfff":{"unused":"\ud800"},
    "projects":[null,{"id":"a","path":"/alpha","future":{"nested":[false,"keep"]}},7],
    "activeProjectId":"a","terminalCloseOnExit":false,"terminalFontSize":23,
    "terminalShell":"pwsh","terminalLoginShells":["pwsh",0,"bash"]
}"#;

#[tokio::test]
async fn project_edit_keeps_nested_metadata_and_sends_only_the_registry() {
    let server = Server::start(
        |_| Reply::with_status(200, "application/json", SETTINGS),
        vec![],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    let mut registry = client.project_entries().await.expect("project list");
    assert_eq!(registry.active_project_id.as_deref(), Some("a"));
    assert_eq!(registry.entries.len(), 1);
    registry.entries[0].insert("label".into(), json!("Changed"));
    client
        .save_project_entries(&registry)
        .await
        .expect("merged settings echo");
    registry.active_project_id = None;
    client
        .save_project_entries(&registry)
        .await
        .expect("no active ID");
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let projects =
        json!([{"id":"a","path":"/alpha","label":"Changed","future":{"nested":[false,"keep"]}}]);
    assert_eq!(
        requests[1].json(),
        json!({"projects":projects,"activeProjectId":"a"})
    );
    assert_eq!(requests[2].json(), json!({"projects":projects}));
}

#[tokio::test]
async fn terminal_reads_each_preference_independently() {
    let valid = TerminalSettings {
        close_on_exit: false,
        font_size: 23.0,
        shell: "pwsh".into(),
        login_shells: vec!["pwsh".into(), "bash".into()],
    };
    for (body, expected) in [
        (SETTINGS, valid),
        (
            r#"{"unused":{"future":["\ud800"]},"terminalFontSize":100,"terminalShell":"  ","terminalLoginShells":false}"#,
            TerminalSettings {
                font_size: 32.0,
                ..TerminalSettings::default()
            },
        ),
        (
            r#"{"terminalFontSize":1,"terminalCloseOnExit":"false"}"#,
            TerminalSettings {
                font_size: 9.0,
                ..TerminalSettings::default()
            },
        ),
    ] {
        let server = Server::start(
            move |_| Reply::with_status(200, "application/json", body),
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        assert_eq!(
            TerminalApi::new(client)
                .settings()
                .await
                .expect("preferences"),
            expected
        );
    }
}

#[tokio::test]
async fn missing_registry_fields_and_invalid_projects_remain_distinct() {
    for body in [
        "{}",
        r#"{"projects":null,"activeProjectId":7}"#,
        r#"{"projects":[],"activeProjectId":""}"#,
    ] {
        let server = Server::start(
            move |_| Reply::with_status(200, "application/json", body),
            vec![],
        )
        .await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        assert_eq!(
            client.project_entries().await.expect("empty registry"),
            ProjectEntries::default()
        );
    }
    let server = Server::start(|_| Reply::json(json!({"projects":{}})), vec![]).await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    assert!(
        matches!(client.project_entries().await, Err(TransportError::Malformed(why)) if why == "the stored projects are not a list")
    );
}

#[tokio::test]
async fn an_unrepresentable_project_entry_cannot_be_silently_rewritten() {
    let server = Server::start(
        |_| {
            Reply::with_status(
                200,
                "application/json",
                r#"{"projects":[{"future":"\ud800"}],"terminalShell":"fish"}"#,
            )
        },
        vec![],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    assert!(matches!(
        client.project_entries().await,
        Err(TransportError::Malformed(_))
    ));
    assert_eq!(
        TerminalApi::new(client)
            .settings()
            .await
            .expect("terminal")
            .shell,
        "fish"
    );
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.method == "GET")
    );
}

#[tokio::test]
async fn project_metadata_keys_are_literal_even_with_raw_value_enabled() {
    let settings = r#"{"pro\u006aects":[{"future":[{"$serde_json::private::RawValue":"42"}]}]}"#;
    let server = Server::start(
        move |_| Reply::with_status(200, "application/json", settings),
        vec![],
    )
    .await;
    let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
    let registry = client.project_entries().await.expect("registry");
    assert_eq!(
        registry.entries[0]["future"][0]["$serde_json::private::RawValue"],
        "42"
    );
    client.save_project_entries(&registry).await.expect("save");
    assert_eq!(
        server.requests()[1].body,
        br#"{"projects":[{"future":[{"$serde_json::private::RawValue":"42"}]}]}"#
    );
}

#[tokio::test]
async fn broken_responses_are_errors_for_all_three_operations() {
    for (status, mime, body) in [
        (503, "application/json", "{}"),
        (200, "text/plain", "{}"),
        (200, "application/json", r#"{"unused":"\uXY12"}"#),
        (200, "application/json", r#"{"\uXY12":false}"#),
        (200, "application/json", r#"{"unused":[true,]}"#),
        (200, "application/json", r#"{"projects":["#),
        (200, "application/json", "{} trailing"),
        (200, "application/json", "[]"),
        (200, "application/json", "null"),
    ] {
        let server = Server::start(move |_| Reply::with_status(status, mime, body), vec![]).await;
        let client = ServerClient::new(server.endpoint(), Credential::None).expect("client");
        let TerminalRequestError::Transport(terminal) = TerminalApi::new(client.clone())
            .settings()
            .await
            .expect_err("terminal")
        else {
            panic!("unexpected terminal refusal")
        };
        for err in [
            client.project_entries().await.expect_err("projects"),
            client
                .save_project_entries(&ProjectEntries::default())
                .await
                .expect_err("save"),
            terminal,
        ] {
            if status == 503 {
                assert!(matches!(err, TransportError::Http { status: 503 }));
            } else {
                assert!(matches!(err, TransportError::Malformed(_)));
            }
        }
        assert_eq!(server.requests().len(), 3);
    }
}
