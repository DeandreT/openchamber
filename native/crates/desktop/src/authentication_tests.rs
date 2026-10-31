//! Sign-in presentation and command guards through GPUI's input dispatch.

use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, Element, ElementId, IntoElement, Render, Role, TestAppContext};

use crate::connection::ConnectionFailure;

#[gpui_kit::test]
fn sign_in_hides_workspace_actions_and_exposes_no_password_value(cx: &mut TestAppContext) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.show_authentication(
                ConnectionFailure::AuthenticationRequired,
                None,
                window,
                cx,
            );
        });
        window.render_frame(cx);
        assert!(
            window
                .try_find(ElementId::Name("composer".into()))
                .is_none()
        );
        assert_eq!(
            window
                .find(ElementId::Name("authentication-unlock".into()))
                .role(),
            Some(Role::Button)
        );
        window.input("private-fixture-password", cx);
        window.render_frame(cx);
        workspace.update(cx, |workspace, cx| {
            let mut node = gpui_kit::accesskit::Node::new(Role::TextInput);
            workspace.authentication.password.update(cx, |input, cx| {
                input
                    .render(window, cx)
                    .into_element()
                    .write_a11y_info(&mut node);
            });
            assert!(
                node.value().is_none(),
                "the secret input never exposes its plaintext value"
            );
        });
    })
    .expect("window");
}

#[gpui_kit::test]
fn pairing_only_failure_never_offers_password_entry(cx: &mut TestAppContext) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.show_authentication(
                ConnectionFailure::ClientAuthenticationRequired,
                None,
                window,
                cx,
            );
        });
        window.render_frame(cx);
        assert!(
            window
                .try_find(ElementId::Name("authentication-password".into()))
                .is_none()
        );
        assert!(
            window
                .try_find(ElementId::Name("authentication-unlock".into()))
                .is_none()
        );
    })
    .expect("window");
}

#[gpui_kit::test]
fn trust_device_changes_through_pointer_and_keyboard_activation(cx: &mut TestAppContext) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.show_authentication(
                ConnectionFailure::AuthenticationRequired,
                None,
                window,
                cx,
            );
        });
        window.render_frame(cx);
        window.click(ElementId::Name("authentication-trust".into()), cx);
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.authentication.trust_device);
            workspace.authentication.trust_focus.focus(window, cx);
        });
        window.render_frame(cx);
        window.press("space", cx);
        workspace.update(cx, |workspace, _| {
            assert!(!workspace.authentication.trust_device)
        });
    })
    .expect("window");
}

#[gpui_kit::test]
fn a_late_connection_after_forgetting_cannot_repopulate_the_token_cache(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    use crate::authentication::{ConnectOutcome, VerifiedConnection};
    use crate::connection::{Connection, ServerConfig};
    use openchamber_native_transport::client::ServerClient;
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::endpoint::Endpoint;
    use openchamber_native_transport::identity::ServerIdentity;

    let (window, workspace) = crate::workspace_tests::open(cx);
    let endpoint = Endpoint::parse("http://127.0.0.1:1/").expect("endpoint");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let lease = workspace
                .authentication
                .store
                .capture(endpoint.as_str())
                .expect("lease");
            workspace
                .authentication
                .store
                .invalidate_endpoint(endpoint.as_str())
                .expect("forgotten");
            workspace.connection = Some(
                Connection::new(ServerConfig::for_endpoint(endpoint.clone())).expect("network"),
            );
            let credential = Credential::ClientToken("oc_client_late".into());
            let client = ServerClient::new(endpoint.clone(), credential.clone()).expect("client");
            workspace.finish_connect(
                0,
                Some(Ok(ConnectOutcome::Ready(Box::new(VerifiedConnection {
                    client,
                    credential,
                    lease: Some(lease),
                    identity: ServerIdentity {
                        openchamber_version: "2.5.3".into(),
                        runtime: "web".into(),
                        server_id: Some("one".into()),
                        api_version: 1,
                        capabilities: Vec::new(),
                    },
                })))),
                window,
                cx,
            );
            assert!(
                !workspace
                    .authentication
                    .remembered
                    .contains_key(endpoint.as_str())
            );
            assert!(
                workspace
                    .connection
                    .as_ref()
                    .expect("connection")
                    .client
                    .is_some(),
                "the open authenticated connection can finish"
            );
            workspace.connection = None;
        });
    })
    .expect("window");
}

#[gpui_kit::test]
fn http_revocation_locks_without_a_websocket_event_and_preserves_the_draft(
    cx: &mut TestAppContext,
) {
    use crate::connection::{Connection, ConnectionState, ServerConfig};
    use openchamber_native_transport::client::{ClientDescription, ServerClient};
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::endpoint::Endpoint;

    let (window, workspace) = crate::workspace_tests::open(cx);
    let endpoint = Endpoint::parse("http://localhost:1/").expect("endpoint");
    let (sender, receiver) = tokio::sync::watch::channel(0);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.composer.update(cx, |input, cx| {
                input.set_value("retained draft", window, cx)
            });
            let mut connection = Connection::new(ServerConfig {
                endpoint: endpoint.clone(),
                password: Some("one-use-fixture".into()),
                client: ClientDescription {
                    label: "fixture".into(),
                    kind: "desktop".into(),
                    dedupe_key: None,
                    device_platform: None,
                    app_version: None,
                },
            })
            .expect("network");
            assert!(connection.take_attempt_config().password.is_some());
            assert!(
                connection.take_attempt_config().password.is_none(),
                "reconnect cannot repeat a password exchange"
            );
            connection.client = Some(
                ServerClient::new(
                    endpoint,
                    Credential::ClientToken("oc_client_fixture".into()),
                )
                .expect("client"),
            );
            connection.state = ConnectionState::Connected;
            workspace.connection = Some(connection);
            workspace.watch_authentication(0, receiver, window, cx);
        });
    })
    .expect("window");
    sender.send(1).expect("active observer");
    cx.run_until_parked();
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert!(
            workspace.authentication.visible,
            "HTTP revocation must open sign-in without an event-stream disconnect"
        );
        assert!(
            workspace
                .connection
                .as_ref()
                .expect("connection")
                .client
                .is_none()
        );
        assert_eq!(workspace.composer.read(cx).value(), "retained draft");
    });
}
