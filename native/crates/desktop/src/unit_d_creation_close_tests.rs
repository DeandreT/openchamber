//! Close regressions through the existing creation fixture and production callbacks.

use super::super::unit_d_close_support::{self as support, ResponseHold};
use super::*;

#[gpui_kit::test]
async fn close_waits_for_creation_preparation_receipt_with_idle_network(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let existing = tests::key("unit-d-preparation", "ses_existing");
    let (server, _) = creation_server(fixture.journal(), Outcome::Success, false);
    let (window, workspace) = open(cx, &server, &fixture, &existing);
    ready(cx, &workspace);
    let mut finish = support::finish_gate(cx, &workspace);
    let (created, mut preparation) = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                prepare(workspace, &existing, window, cx);
                let gate = workspace
                    .attachment_worker()
                    .expect("live worker")
                    .hold_next_snapshot_reply_for_test();
                workspace.submit(window, cx);
                let key = workspace
                    .creations
                    .iter()
                    .next()
                    .expect("real accepted creation")
                    .0
                    .clone();
                (key, gate)
            })
        })
        .expect("creation submits under one foreground transition");
    preparation.reached().await;
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace
                .read(cx)
                .connection
                .as_ref()
                .expect("captured connection")
                .network
                .is_idle()
        })
    });
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert!(workspace.creation_tasks.contains_key(&created));
        assert_eq!(
            workspace
                .creations
                .get(&created)
                .expect("capture retained")
                .phase,
            CreationPhase::Preparing
        );
    });
    assert_eq!(
        create_count(&server),
        0,
        "the durable preparation reply still precedes POST"
    );
    support::request_close(cx, window, &workspace);
    support::assert_live_journal_wait(cx, window, &workspace, &finish).await;
    cx.read(|cx| {
        assert!(
            workspace.read(cx).creation_tasks.contains_key(&created),
            "close preserves recovery exclusion during the receipt wait"
        )
    });
    preparation.release();
    finish.reached().await;
    assert_eq!(
        create_count(&server),
        1,
        "accepted creation completes exactly once after release"
    );
    assert_eq!(
        server.prompt_count(),
        1,
        "the accepted first-send continuation also completes"
    );
    let (_, loaded) =
        MutationJournal::open(&fixture.journal()).expect("actual finish released the journal lock");
    let LoadedJournal::Present(snapshot) = loaded else {
        panic!("real final snapshot exists");
    };
    assert!(
        snapshot.creations.is_empty(),
        "final snapshot contains the creation handoff"
    );
    assert!(snapshot.attempts.iter().any(|(key, attempt)| key == &created && attempt.prompt.text == "first creation prompt"), "the malformed fixture prompt receipt retains exact recovery custody");
    support::close_after_finish(cx, window, &finish);
}

#[gpui_kit::test]
async fn close_waits_for_same_key_creation_observation_successor(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let existing = tests::key("unit-d-successor", "ses_existing");
    // Declare the server first so panic drops reply releases before joining it.
    let server;
    let (create_hold, create_gate) = ResponseHold::exact("POST", "/api/session");
    let (lookup_hold, lookup_gate) = ResponseHold::session_lookup();
    server = creation_server_with_holds(
        fixture.journal(),
        Outcome::Rejected,
        false,
        vec![create_gate, lookup_gate],
    )
    .0;
    let (window, workspace) = open(cx, &server, &fixture, &existing);
    ready(cx, &workspace);
    let mut finish = support::finish_gate(cx, &workspace);
    let created = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                prepare(workspace, &existing, window, cx);
                workspace.submit(window, cx);
                workspace
                    .creations
                    .iter()
                    .next()
                    .expect("accepted creation")
                    .0
                    .clone()
            })
        })
        .expect("creation starts");
    wait(cx, |_| create_hold.has_reached());
    support::request_close(cx, window, &workspace);
    support::assert_live_journal_wait(cx, window, &workspace, &finish).await;
    create_hold.release();
    wait(cx, |_| lookup_hold.has_reached());
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            assert!(
                workspace.creation_tasks.contains_key(&created),
                "the production error callback installed a same-key observation"
            );
            assert!(
                workspace.creations.checks.get(&created)
                    == Some(&crate::create_attempt::CreationCheck::Checking),
                "the successor owns the real observation state"
            );
            workspace.check_creation(&created, false, window, cx);
            assert!(
                workspace.creation_tasks.contains_key(&created),
                "duplicate recovery cannot replace the held successor"
            );
        })
    })
    .expect("same-key successor retains its window");
    support::assert_live_journal_wait(cx, window, &workspace, &finish).await;
    lookup_hold.release();
    finish.reached().await;
    cx.read(|cx| {
        assert!(
            workspace.read(cx).creation_tasks.is_empty(),
            "successor callback completed before sealing"
        )
    });
    let (_, loaded) =
        MutationJournal::open(&fixture.journal()).expect("finish released the real writer");
    let LoadedJournal::Present(snapshot) = loaded else {
        panic!("saved creation exists");
    };
    assert!(
        snapshot
            .creations
            .iter()
            .any(|(key, attempt)| key == &created && attempt.phase == CreationPhase::Uncertain)
    );
    assert_eq!(
        create_count(&server),
        1,
        "observation never retries the rejected create"
    );
    assert_eq!(server.prompt_count(), 0);
    support::close_after_finish(cx, window, &finish);
}

#[gpui_kit::test]
async fn close_waits_for_create_to_first_send_continuation(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let existing = tests::key("unit-d-first-send", "ses_existing");
    let server;
    let (create_hold, create_gate) = ResponseHold::exact("POST", "/api/session");
    let (prompt_hold, prompt_gate) = ResponseHold::suffix("POST", "/prompt");
    server = creation_server_with_holds(
        fixture.journal(),
        Outcome::Success,
        false,
        vec![create_gate, prompt_gate],
    )
    .0;
    let (window, workspace) = open(cx, &server, &fixture, &existing);
    ready(cx, &workspace);
    let mut finish = support::finish_gate(cx, &workspace);
    let (created, message) = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                prepare(workspace, &existing, window, cx);
                workspace.submit(window, cx);
                let (key, attempt) = workspace.creations.iter().next().expect("creation capture");
                (key.clone(), attempt.prompt.message_id.clone())
            })
        })
        .expect("real root creation starts");
    wait(cx, |_| create_hold.has_reached());
    support::request_close(cx, window, &workspace);
    support::assert_live_journal_wait(cx, window, &workspace, &finish).await;
    create_hold.release();
    wait(cx, |_| prompt_hold.has_reached());
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert!(
            workspace.creations.get(&created).is_none(),
            "the real creation callback handed ownership over"
        );
        assert!(
            workspace.mutation_tasks.contains_key(&created),
            "the first send remains accepted under the original runtime"
        );
        assert_eq!(
            workspace
                .send_attempts
                .attempt(&created, &message)
                .expect("bound first prompt")
                .origin,
            SendOrigin::Created
        );
    });
    support::assert_live_journal_wait(cx, window, &workspace, &finish).await;
    prompt_hold.release();
    finish.reached().await;
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert!(workspace.creation_tasks.is_empty() && workspace.mutation_tasks.is_empty());
        assert!(!workspace.send_attempts.is_active(&created));
    });
    let (_, loaded) = MutationJournal::open(&fixture.journal())
        .expect("successful finish released the actual lock");
    let LoadedJournal::Present(snapshot) = loaded else {
        panic!("first-send journal exists");
    };
    assert!(snapshot.creations.is_empty());
    assert!(
        snapshot
            .attempts
            .iter()
            .any(|(key, attempt)| key == &created
                && attempt.prompt.message_id == message
                && attempt.admission == Admission::Uncertain)
    );
    assert_eq!(create_count(&server), 1);
    assert_eq!(server.prompt_count(), 1);
    support::close_after_finish(cx, window, &finish);
}
