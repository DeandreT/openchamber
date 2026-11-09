//! Positive control for the test-only preparation reply seam using actual disk I/O.

use super::*;

#[gpui_kit::test]
async fn held_snapshot_reply_does_not_hold_flush_or_real_finish(cx: &mut gpui_kit::TestAppContext) {
    cx.executor().allow_parking();
    let fixture = crate::recovery_tests::Fixture::new();
    let (worker, hydration) = JournalWorker::start(fixture.journal());
    hydration
        .await
        .expect("real hydration reply")
        .expect("private journal opens");
    let mut held = worker.hold_next_snapshot_reply_for_test();
    let mut snapshot = worker.submit(JournalSnapshot::default());
    held.reached().await;
    assert!(
        matches!(
            snapshot.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ),
        "only the computed snapshot reply is held"
    );
    assert_eq!(
        worker.flush().await.expect("independent real checkpoint"),
        Ok(CommitOutcome::Durable)
    );
    assert!(
        matches!(
            crate::mutation_journal::MutationJournal::open(&fixture.journal()),
            Err(JournalError::Locked)
        ),
        "real writer still owns the journal before finish"
    );
    assert_eq!(
        worker
            .finish()
            .await
            .expect("real finish can proceed with an externally held reply"),
        Ok(CommitOutcome::Durable)
    );
    let (journal, loaded) = crate::mutation_journal::MutationJournal::open(&fixture.journal())
        .expect("real finish dropped the writer and lock");
    assert!(matches!(loaded, LoadedJournal::Present(_)));
    drop(journal);
    assert!(matches!(
        snapshot.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    held.release();
    assert_eq!(
        snapshot
            .await
            .expect("exact computed snapshot result is released"),
        Ok(CommitOutcome::Durable)
    );
}
