//! Accepted permission writes through the existing policy server and Workspace close.

use super::super::unit_d_close_support as support;
use super::*;

#[gpui_kit::test]
async fn close_waits_for_held_permission_write_and_its_foreground_completion(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("unit-d-permission", "ses_permission_close");
    let policy = Arc::new(Mutex::new(Policy {
        hold_writes: true,
        ..Policy::default()
    }));
    let server = policy_server(Arc::clone(&policy));
    let (window, workspace) = open_policy(cx, &server, &fixture, &key);
    click_shield(cx, window);
    wait(cx, |_| {
        count(&server, "PUT", "/api/permission-auto-accept/sessions/") == 1
    });
    cx.read(|cx| assert_eq!(workspace.read(cx).permission_modes.writes_in_flight(), 1));
    let mut finish = support::finish_gate(cx, &workspace);
    support::request_close(cx, window, &workspace);
    support::assert_live_journal_wait(cx, window, &workspace, &finish).await;
    policy.lock().expect("policy").hold_writes = false;
    finish.reached().await;
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert_eq!(
            workspace.permission_modes.writes_in_flight(),
            0,
            "the accepted write's actual callback precedes final finish"
        );
        assert_eq!(
            workspace.session_permission_mode(&key.session),
            PermissionMode::Auto
        );
    });
    assert_eq!(
        writes(&server).len(),
        1,
        "close never resubmits a permission choice"
    );
    assert_eq!(
        policy
            .lock()
            .expect("policy")
            .modes
            .get(key.session.as_str()),
        Some(&json!("auto"))
    );
    support::close_after_finish(cx, window, &finish);
}

#[gpui_kit::test]
fn no_loaded_journal_close_still_waits_for_an_accepted_permission_write(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let key = tests::key("unit-d-no-journal", "ses_no_journal");
    let policy = Arc::new(Mutex::new(Policy {
        hold_writes: true,
        ..Policy::default()
    }));
    let server = policy_server(Arc::clone(&policy));
    // Start from the maintained synthetic window fixture, then attach the same
    // real loopback connection used by recovery_tests::open. No journal is opened,
    // detached, or assigned a fabricated persistence status.
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let config = ServerConfig {
                endpoint: server.endpoint.clone(),
                password: None,
                client: ClientDescription::default(),
            };
            let mut connection = Connection::new(config).expect("owned fixture executor");
            connection.client = Some(
                ServerClient::new(server.endpoint.clone(), Credential::None)
                    .expect("anonymous loopback client"),
            );
            connection.state = ConnectionState::Connected;
            workspace.connection = Some(connection);
            workspace.switch_runtime(key.runtime.clone(), window, cx);
            configure_send(workspace, &key, window, cx);
            assert!(!workspace.persistence_loaded());
            assert!(workspace.attachment_worker().is_none());
        })
    })
    .expect("real connected window has no loaded journal");
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            workspace.send_catalog.routing_read.is_none()
                && workspace.permission_modes.policy.is_loaded()
        })
    });
    click_shield(cx, window);
    wait(cx, |_| {
        count(&server, "PUT", "/api/permission-auto-accept/sessions/") == 1
    });
    cx.read(|cx| assert_eq!(workspace.read(cx).permission_modes.writes_in_flight(), 1));
    support::request_close(cx, window, &workspace);
    assert!(
        cx.update_window(window, |_, _, _| ()).is_ok(),
        "the no-loaded-journal branch cannot remove an accepted writer's window"
    );
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert!(workspace.close_in_progress());
        assert!(!workspace.persistence_loaded());
        assert!(workspace.attachment_worker().is_none());
    });
    policy.lock().expect("policy").hold_writes = false;
    wait(cx, |cx| cx.update_window(window, |_, _, _| ()).is_err());
    cx.read(|cx| {
        assert_eq!(
            workspace.read(cx).permission_modes.writes_in_flight(),
            0,
            "the actual callback completes before removal"
        )
    });
    assert_eq!(
        policy
            .lock()
            .expect("policy")
            .modes
            .get(key.session.as_str()),
        Some(&json!("auto"))
    );
    assert_eq!(writes(&server).len(), 1);
}

#[gpui_kit::test]
async fn accepted_close_rejects_a_fresh_stop_without_interrupting_accepted_work(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("unit-d-close-stop", "ses_close_stop");
    let policy = Arc::new(Mutex::new(Policy {
        hold_writes: true,
        ..Policy::default()
    }));
    let server = policy_server(Arc::clone(&policy));
    let (window, workspace) = open_policy(cx, &server, &fixture, &key);
    click_shield(cx, window);
    wait(cx, |_| {
        count(&server, "PUT", "/api/permission-auto-accept/sessions/") == 1
    });
    let mut finish = support::finish_gate(cx, &workspace);
    support::request_close(cx, window, &workspace);
    support::assert_live_journal_wait(cx, window, &workspace, &finish).await;
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.abort_tasks.is_empty());
            assert!(!workspace.send_attempts.is_active(&key));
            workspace.stop(window, cx);
            assert!(
                workspace.abort_tasks.is_empty(),
                "close admits no new Stop task"
            );
            assert!(
                !workspace.send_attempts.is_active(&key),
                "Stop cannot acquire abort admission during close"
            );
            assert_eq!(
                workspace.permission_modes.writes_in_flight(),
                1,
                "the existing accepted write remains owned"
            );
        });
    })
    .expect("accepted work retains the live window");
    policy.lock().expect("policy").hold_writes = false;
    finish.reached().await;
    assert_eq!(
        count(&server, "POST", "/api/session/"),
        0,
        "no interrupt request leaves during close"
    );
    assert_eq!(writes(&server).len(), 1);
    support::close_after_finish(cx, window, &finish);
}

#[gpui_kit::test]
async fn accepted_close_rejects_a_fresh_permission_choice(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("unit-d-close-mode", "ses_close_mode");
    let policy = Arc::new(Mutex::new(Policy {
        hold_writes: true,
        ..Policy::default()
    }));
    let server = policy_server(Arc::clone(&policy));
    let (window, workspace) = open_policy(cx, &server, &fixture, &key);
    click_shield(cx, window);
    wait(cx, |_| {
        count(&server, "PUT", "/api/permission-auto-accept/sessions/") == 1
    });
    let mut finish = support::finish_gate(cx, &workspace);
    support::request_close(cx, window, &workspace);
    support::assert_live_journal_wait(cx, window, &workspace, &finish).await;
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            assert_eq!(workspace.permission_modes.writes_in_flight(), 1);
            workspace.cycle_permission_mode(window, cx);
            assert_eq!(
                workspace.permission_modes.writes_in_flight(),
                1,
                "a fresh permission gesture cannot enqueue another write during close"
            );
        });
    })
    .expect("accepted permission callback keeps its window");
    policy.lock().expect("policy").hold_writes = false;
    finish.reached().await;
    assert_eq!(
        writes(&server).len(),
        1,
        "only the pre-close choice was submitted"
    );
    cx.read(|cx| {
        assert_eq!(
            workspace.read(cx).session_permission_mode(&key.session),
            PermissionMode::Auto
        )
    });
    support::close_after_finish(cx, window, &finish);
}
