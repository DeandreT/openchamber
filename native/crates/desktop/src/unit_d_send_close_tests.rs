//! Close regressions through real send/recovery tasks and the maintained scripted server.

use super::super::unit_d_close_support::{self as support, ResponseHold};
use super::*;

#[gpui_kit::test]
async fn final_journal_contains_the_accepted_send_callback_rollback(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("unit-d-callback", "ses_callback");
    // Release channels drop before the fixture server joins during panic cleanup.
    let scripted;
    let (prompt_hold, prompt_gate) = ResponseHold::suffix("POST", "/prompt");
    scripted = Scripted::with_holds(
        fixture.journal(),
        Script {
            refuse: vec!["prompt"],
            ..Script::default()
        },
        vec![prompt_gate],
    );
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx)
        })
    })
    .expect("existing session is configured");
    type_and(
        cx,
        window,
        &workspace,
        "restore this exact accepted text",
        None,
    );
    wait(cx, |_| prompt_hold.has_reached());
    let (owner, message) = cx.read(|cx| {
        let workspace = workspace.read(cx);
        let attempt = workspace
            .send_attempts
            .session(&key)
            .next()
            .expect("real captured send");
        (attempt.owner.clone(), attempt.prompt.message_id.clone())
    });
    let mut finish = support::finish_gate(cx, &workspace);
    support::request_close(cx, window, &workspace);
    support::assert_live_journal_wait(cx, window, &workspace, &finish).await;
    // The real rejection stays held until close has proved the journal remains
    // writable. Its actual callback must publish rollback before final finish.
    prompt_hold.release();
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            workspace.mutation_tasks.is_empty() && !workspace.send_attempts.is_active(&key)
        })
    });
    finish.reached().await;
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert!(workspace.send_attempts.attempt(&key, &message).is_none());
        assert_eq!(
            workspace
                .drafts
                .get(&owner)
                .expect("real rejection restores draft")
                .text,
            "restore this exact accepted text"
        );
    });
    let (_, loaded) = MutationJournal::open(&fixture.journal())
        .expect("the held finish reply follows actual writer release");
    let LoadedJournal::Present(snapshot) = loaded else {
        panic!("real final snapshot exists");
    };
    assert!(
        snapshot
            .attempts
            .iter()
            .all(|(saved_key, attempt)| saved_key != &key || attempt.prompt.message_id != message),
        "the final snapshot includes the callback's attempt removal"
    );
    assert!(
        snapshot
            .drafts
            .iter()
            .any(|(saved_owner, draft)| saved_owner == &owner
                && draft.text == "restore this exact accepted text"),
        "the final snapshot includes the callback's restored draft"
    );
    support::close_after_finish(cx, window, &finish);
}

#[gpui_kit::test]
async fn close_waits_for_accepted_recovery_observation_and_retry_continuation(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("unit-d-recovery", "ses_recovery_close");
    fixture.seed(&key);
    let inbox_path = format!("/api/session/{}/inbox", key.session);
    let scripted;
    let (observation_hold, observation_gate) = ResponseHold::exact("GET", &inbox_path);
    let (prompt_hold, prompt_gate) = ResponseHold::suffix("POST", "/prompt");
    scripted = Scripted::with_holds(
        fixture.journal(),
        Script::default(),
        vec![observation_gate, prompt_gate],
    );
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    let message = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                configure_send(workspace, &key, window, cx);
                let ids = workspace.send_attempts.ids(&key);
                assert_eq!(
                    ids.len(),
                    1,
                    "the real journal hydrated one unresolved identity"
                );
                let id = ids[0].clone();
                workspace.retry_recovery(&key, &id, window, cx);
                id
            })
        })
        .expect("explicit Retry same message starts its real observation");
    wait(cx, |_| observation_hold.has_reached());
    assert!(
        scripted.posts().is_empty(),
        "observation has not dispatched a prompt"
    );
    let mut finish = support::finish_gate(cx, &workspace);
    support::request_close(cx, window, &workspace);
    support::assert_live_journal_wait(cx, window, &workspace, &finish).await;
    observation_hold.release();
    wait(cx, |_| prompt_hold.has_reached());
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert!(
            workspace.mutation_tasks.contains_key(&key),
            "the accepted recovery callback installed its actual retry chain"
        );
        assert!(workspace.send_attempts.is_active(&key));
        assert!(workspace.send_attempts.attempt(&key, &message).is_some());
    });
    support::assert_live_journal_wait(cx, window, &workspace, &finish).await;
    prompt_hold.release();
    finish.reached().await;
    let prompts: Vec<Value> = scripted
        .requests
        .lock()
        .expect("requests")
        .iter()
        .filter(|request| request.method == "POST" && request.path.ends_with("/prompt"))
        .map(|request| serde_json::from_slice(&request.body).expect("actual prompt JSON"))
        .collect();
    assert_eq!(
        prompts.len(),
        1,
        "close does not replay or duplicate the accepted retry"
    );
    assert_eq!(prompts[0]["id"].as_str(), Some(message.as_str()));
    assert_eq!(prompts[0]["text"], "submitted text");
    cx.read(|cx| {
        assert!(
            workspace.read(cx).mutation_tasks.is_empty(),
            "retry callback finishes before final journal finish"
        )
    });
    let (_, loaded) =
        MutationJournal::open(&fixture.journal()).expect("real finish releases the lock");
    let LoadedJournal::Present(snapshot) = loaded else {
        panic!("retry snapshot exists");
    };
    assert!(
        snapshot
            .attempts
            .iter()
            .any(|(saved_key, attempt)| saved_key == &key && attempt.prompt.message_id == message),
        "exact retry recovery custody survives final save"
    );
    support::close_after_finish(cx, window, &finish);
}

#[gpui_kit::test]
async fn accepted_close_rejects_a_fresh_retry_without_starting_observation(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("unit-d-close-retry", "ses_close_retry");
    fixture.seed(&key);
    let scripted;
    let mode_path = format!("/api/permission-auto-accept/sessions/{}", key.session);
    let (mode_hold, mode_gate) = ResponseHold::exact("PUT", &mode_path);
    scripted = Scripted::with_holds(fixture.journal(), Script::default(), vec![mode_gate]);
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    let message = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                configure_send(workspace, &key, window, cx);
                let id = workspace.send_attempts.ids(&key)[0].clone();
                workspace.check_recovery(&key, &id, window, cx);
                id
            })
        })
        .expect("the existing retained prompt is observed through its real handler");
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace.read(cx).recovery_items(cx).iter().any(|row| {
                row.key == key
                    && row.message == message
                    && row.phase == crate::recovery_view::RecoveryPhase::NotObserved
            })
        })
    });
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let row = workspace
                .recovery_items(cx)
                .into_iter()
                .find(|row| row.key == key && row.message == message)
                .expect("observed recovery row");
            assert!(
                row.can_retry,
                "the fixture's exact prompt is eligible before close"
            );
            workspace.cycle_permission_mode(window, cx);
        });
    })
    .expect("a separate permission write is accepted before close");
    wait(cx, |_| mode_hold.has_reached());
    let mut finish = support::finish_gate(cx, &workspace);
    support::request_close(cx, window, &workspace);
    support::assert_live_journal_wait(cx, window, &workspace, &finish).await;
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.retry_recovery(&key, &message, window, cx);
            let row = workspace
                .recovery_items(cx)
                .into_iter()
                .find(|row| row.key == key && row.message == message)
                .expect("exact recovery custody remains");
            assert_eq!(
                row.phase,
                crate::recovery_view::RecoveryPhase::NotObserved,
                "a fresh retry cannot enter Checking while accepted close is pending"
            );
            assert!(!workspace.send_attempts.is_active(&key));
            assert_eq!(workspace.permission_modes.writes_in_flight(), 1);
        });
    })
    .expect("the existing accepted write retains its window");
    mode_hold.release();
    finish.reached().await;
    assert!(
        scripted.posts().is_empty(),
        "close never dispatches a fresh retry prompt"
    );
    cx.read(|cx| {
        assert!(
            workspace
                .read(cx)
                .send_attempts
                .attempt(&key, &message)
                .is_some(),
            "refusing fresh retry preserves its original recovery attempt"
        )
    });
    support::close_after_finish(cx, window, &finish);
}
