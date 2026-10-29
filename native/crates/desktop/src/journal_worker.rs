//! Serialized journal I/O on a dedicated thread. No method performs file I/O
//! or waits for queue capacity on the calling thread.
//!
//! Submit full snapshots in mutation order after hydration succeeds. There is
//! one in-flight snapshot, one replaceable pending snapshot, and at most 128
//! outstanding save/flush barriers. Superseded pending snapshots keep their
//! barriers: the newest snapshot's durable commit satisfies them all. Callers
//! must check that a send is still current after awaiting its barrier.
//!
//! Failure clears queued work and fails its barriers. A subsequent submit is
//! an explicit retry. Flush reports the latest accepted work's outcome; finish
//! seals admission, drains accepted work and releases the file lock before its
//! receipt. Dropping the sole handle also drains accepted work without joining.
//! A canceled receipt means the worker stopped without confirming durability.

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use tokio::sync::oneshot;

use crate::mutation_journal::{
    JournalCommit, JournalError, JournalSnapshot, LoadedJournal, MutationJournal,
};

const MAX_WAITERS: usize = 128;

/// Contains only classifications, never paths, draft text or provider data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum JournalFailure {
    Io(io::ErrorKind),
    Locked,
    Invalid(&'static str),
    UnsupportedVersion(u32),
    NotHydrated,
    AtCapacity,
    Closed,
    WorkerStopped,
}

impl From<JournalError> for JournalFailure {
    fn from(error: JournalError) -> Self {
        match error {
            JournalError::Io(error) => Self::Io(error.kind()),
            JournalError::Locked => Self::Locked,
            JournalError::Invalid(reason) => Self::Invalid(reason),
            JournalError::UnsupportedVersion(version) => Self::UnsupportedVersion(version),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommitOutcome {
    Durable,
    DirectorySyncUncertain(io::ErrorKind),
}

pub(crate) type JournalResult = Result<CommitOutcome, JournalFailure>;
pub(crate) type JournalReceipt = oneshot::Receiver<JournalResult>;
type Waiter = oneshot::Sender<JournalResult>;
pub(crate) type HydrationReceipt = oneshot::Receiver<Result<LoadedJournal, JournalFailure>>;

enum Phase {
    Opening,
    Ready,
    Failed(JournalFailure),
}

struct Pending {
    snapshot: JournalSnapshot,
    waiters: Vec<Waiter>,
}

struct State {
    phase: Phase,
    hydration: Option<oneshot::Sender<Result<LoadedJournal, JournalFailure>>>,
    pending: Option<Pending>,
    active: Vec<Waiter>,
    writing: bool,
    closing: bool,
    stopped: bool,
    finish: Option<Waiter>,
    last: JournalResult,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// A single ownership handle. Dropping it requests graceful asynchronous stop.
pub(crate) struct JournalWorker {
    shared: Arc<Shared>,
}

impl JournalWorker {
    pub(crate) fn start(path: PathBuf) -> (Self, HydrationReceipt) {
        Self::start_with(move || MutationJournal::open(&path), MutationJournal::save)
    }

    fn start_with<W, O, S>(open: O, save: S) -> (Self, HydrationReceipt)
    where
        W: Send + 'static,
        O: FnOnce() -> Result<(W, LoadedJournal), JournalError> + Send + 'static,
        S: FnMut(&mut W, &JournalSnapshot) -> Result<JournalCommit, JournalError> + Send + 'static,
    {
        let (hydration, receipt) = oneshot::channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                phase: Phase::Opening,
                hydration: Some(hydration),
                pending: None,
                active: Vec::new(),
                writing: false,
                closing: false,
                stopped: false,
                finish: None,
                last: Ok(CommitOutcome::Durable),
            }),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let spawned = thread::Builder::new()
            .name("native-journal".to_owned())
            .spawn(move || {
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    run(&worker_shared, open, save);
                }));
                if result.is_err() {
                    stop_failed(&worker_shared, JournalFailure::WorkerStopped);
                }
            });
        if let Err(error) = spawned {
            stop_failed(&shared, JournalFailure::Io(error.kind()));
        }
        (Self { shared }, receipt)
    }

    /// Rejects pre-hydration, closed and over-capacity submissions immediately.
    /// The rejected snapshot never replaces accepted pending work.
    pub(crate) fn submit(&self, snapshot: JournalSnapshot) -> JournalReceipt {
        let (waiter, receipt) = oneshot::channel();
        let Ok(mut state) = self.shared.state.lock() else {
            let _ = waiter.send(Err(JournalFailure::WorkerStopped));
            return receipt;
        };
        if let Some(error) = admission_error(&state) {
            let _ = waiter.send(Err(error));
            return receipt;
        }
        let replaced = if let Some(pending) = &mut state.pending {
            pending.waiters.push(waiter);
            Some(std::mem::replace(&mut pending.snapshot, snapshot))
        } else {
            state.pending = Some(Pending {
                snapshot,
                waiters: vec![waiter],
            });
            None
        };
        drop(state);
        self.shared.wake.notify_one();
        // Releasing replaced payload memory does not hold the queue mutex.
        drop(replaced);
        receipt
    }

    /// Waits for all work accepted before this call. Later snapshots can
    /// supersede that work and therefore delay this barrier until they commit.
    pub(crate) fn flush(&self) -> JournalReceipt {
        let (waiter, receipt) = oneshot::channel();
        let Ok(mut state) = self.shared.state.lock() else {
            let _ = waiter.send(Err(JournalFailure::WorkerStopped));
            return receipt;
        };
        if let Some(error) = admission_error(&state) {
            let _ = waiter.send(Err(error));
        } else if let Some(pending) = &mut state.pending {
            pending.waiters.push(waiter);
        } else if state.writing {
            state.active.push(waiter);
        } else {
            let _ = waiter.send(state.last.clone());
        }
        receipt
    }

    /// Stops accepting work. The receipt resolves after accepted writes finish
    /// and the writer drops, releasing its journal lock. Only the first finish
    /// call is accepted. This dedicated receipt does not consume queue capacity.
    pub(crate) fn finish(&self) -> JournalReceipt {
        let (waiter, receipt) = oneshot::channel();
        let Ok(mut state) = self.shared.state.lock() else {
            let _ = waiter.send(Err(JournalFailure::WorkerStopped));
            return receipt;
        };
        if state.closing {
            let _ = waiter.send(Err(JournalFailure::Closed));
        } else if state.stopped {
            state.closing = true;
            let _ = waiter.send(state.last.clone());
        } else {
            state.closing = true;
            state.finish = Some(waiter);
            self.shared.wake.notify_one();
        }
        receipt
    }
}

impl Drop for JournalWorker {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.closing = true;
            self.shared.wake.notify_one();
        }
    }
}

fn admission_error(state: &State) -> Option<JournalFailure> {
    if state.closing {
        return Some(JournalFailure::Closed);
    }
    match &state.phase {
        Phase::Opening => return Some(JournalFailure::NotHydrated),
        Phase::Failed(error) => return Some(error.clone()),
        Phase::Ready => {}
    }
    if state.stopped {
        return Some(JournalFailure::WorkerStopped);
    }
    let pending = state
        .pending
        .as_ref()
        .map_or(0, |pending| pending.waiters.len());
    if state.active.len() + pending >= MAX_WAITERS {
        return Some(JournalFailure::AtCapacity);
    }
    None
}

fn run<W, O, S>(shared: &Shared, open: O, mut save: S)
where
    O: FnOnce() -> Result<(W, LoadedJournal), JournalError>,
    S: FnMut(&mut W, &JournalSnapshot) -> Result<JournalCommit, JournalError>,
{
    let (mut writer, loaded) = match open() {
        Ok(result) => result,
        Err(error) => {
            stop_failed(shared, error.into());
            return;
        }
    };
    {
        let mut state = shared.state.lock().expect("journal state is not poisoned");
        state.phase = Phase::Ready;
        if let Some(hydration) = state.hydration.take() {
            let _ = hydration.send(Ok(loaded));
        }
    }
    loop {
        let snapshot = {
            let mut state = shared.state.lock().expect("journal state is not poisoned");
            while state.pending.is_none() && !state.closing {
                state = shared
                    .wake
                    .wait(state)
                    .expect("journal state is not poisoned");
            }
            let Some(pending) = state.pending.take() else {
                break;
            };
            state.active = pending.waiters;
            state.writing = true;
            pending.snapshot
        };
        let outcome = match save(&mut writer, &snapshot) {
            Ok(JournalCommit::Durable) => Ok(CommitOutcome::Durable),
            Ok(JournalCommit::DirectorySyncUncertain(error)) => {
                Ok(CommitOutcome::DirectorySyncUncertain(error.kind()))
            }
            Err(error) => Err(error.into()),
        };
        drop(snapshot);
        let mut state = shared.state.lock().expect("journal state is not poisoned");
        state.writing = false;
        state.last = outcome.clone();
        let waiters = std::mem::take(&mut state.active);
        let failed_pending = if outcome == Ok(CommitOutcome::Durable) {
            None
        } else {
            state.pending.take()
        };
        // Publish outcomes while holding the ordering lock. A new explicit
        // retry submitted after a failure receipt cannot be cleared by it.
        respond(waiters, &outcome);
        if let Some(pending) = failed_pending {
            respond(pending.waiters, &outcome);
            drop(state);
            drop(pending.snapshot);
        }
    }
    drop(writer);
    let mut state = shared.state.lock().expect("journal state is not poisoned");
    state.stopped = true;
    if let Some(waiter) = state.finish.take() {
        let _ = waiter.send(state.last.clone());
    }
}

fn respond(waiters: Vec<Waiter>, outcome: &JournalResult) {
    for waiter in waiters {
        let _ = waiter.send(outcome.clone());
    }
}

fn stop_failed(shared: &Shared, error: JournalFailure) {
    let Ok(mut state) = shared.state.lock() else {
        return;
    };
    state.phase = Phase::Failed(error.clone());
    state.stopped = true;
    state.writing = false;
    state.last = Err(error.clone());
    if let Some(hydration) = state.hydration.take() {
        let _ = hydration.send(Err(error));
    }
    respond(std::mem::take(&mut state.active), &state.last);
    let pending = state.pending.take();
    if let Some(waiter) = state.finish.take() {
        let _ = waiter.send(state.last.clone());
    }
    let outcome = state.last.clone();
    drop(state);
    if let Some(pending) = pending {
        respond(pending.waiters, &outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
    use openchamber_native_core::ids::{DraftId, RuntimeId};
    use std::sync::mpsc;

    fn snapshot(text: &str) -> JournalSnapshot {
        JournalSnapshot {
            drafts: vec![(
                DraftKey::new(
                    RuntimeId::from("runtime"),
                    "/project",
                    DraftTarget::NewSession(DraftId::from("draft")),
                )
                .expect("valid owner"),
                Draft::new(text),
            )],
            attempts: Vec::new(),
            creations: Vec::new(),
            selections: Vec::new(),
        }
    }

    fn receive(receipt: JournalReceipt) -> JournalResult {
        receipt.blocking_recv().expect("worker answers receipt")
    }

    #[test]
    fn coalescing_preserves_order_and_all_barriers_and_finish_drains() {
        let (entered, entries) = mpsc::channel();
        let (release, permits) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with(
            || Ok(((), LoadedJournal::Missing)),
            move |_, snapshot| {
                entered
                    .send(snapshot.drafts[0].1.text.clone())
                    .expect("test receiver");
                permits.recv().expect("test release");
                Ok(JournalCommit::Durable)
            },
        );
        assert!(matches!(
            hydration.blocking_recv(),
            Ok(Ok(LoadedJournal::Missing))
        ));
        let first = worker.submit(snapshot("first"));
        assert_eq!(entries.recv().expect("first started"), "first");
        let mut second = worker.submit(snapshot("second"));
        let mut third = worker.submit(snapshot("third"));
        let flush = worker.flush();
        let finish = worker.finish();
        assert_eq!(
            receive(worker.submit(snapshot("rejected"))),
            Err(JournalFailure::Closed)
        );
        release.send(()).expect("release first");
        assert_eq!(receive(first), Ok(CommitOutcome::Durable));
        assert_eq!(entries.recv().expect("latest pending started"), "third");
        assert!(matches!(
            second.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            third.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        release.send(()).expect("release third");
        for receipt in [second, third, flush, finish] {
            assert_eq!(receive(receipt), Ok(CommitOutcome::Durable));
        }
        assert!(
            entries.recv().is_err(),
            "writer stopped after exactly two saves"
        );
    }

    #[test]
    fn prehydration_and_open_failure_fail_closed() {
        let (release, permit) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with(
            move || {
                permit.recv().expect("release open");
                Err::<((), LoadedJournal), _>(JournalError::Locked)
            },
            |_, _| panic!("failed open cannot save"),
        );
        assert_eq!(
            receive(worker.submit(snapshot("early"))),
            Err(JournalFailure::NotHydrated)
        );
        release.send(()).expect("release open failure");
        assert!(matches!(
            hydration.blocking_recv(),
            Ok(Err(JournalFailure::Locked))
        ));
        assert_eq!(
            receive(worker.submit(snapshot("late"))),
            Err(JournalFailure::Locked)
        );
        assert_eq!(receive(worker.finish()), Err(JournalFailure::Locked));
    }

    #[test]
    fn failures_cancel_pending_barriers_and_allow_explicit_retry() {
        for uncertain in [false, true] {
            let (entered, entries) = mpsc::channel();
            let (release, permits) = mpsc::channel();
            let mut writes = 0;
            let (worker, hydration) = JournalWorker::start_with(
                || Ok(((), LoadedJournal::Missing)),
                move |_, _| {
                    writes += 1;
                    if writes == 1 {
                        entered.send(()).expect("entered");
                        permits.recv().expect("release failure");
                        if uncertain {
                            return Ok(JournalCommit::DirectorySyncUncertain(io::Error::other(
                                "private context",
                            )));
                        }
                        return Err(JournalError::Io(io::Error::other("private context")));
                    }
                    Ok(JournalCommit::Durable)
                },
            );
            assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
            let first = worker.submit(snapshot("first"));
            entries.recv().expect("first writing");
            let pending = worker.submit(snapshot("pending"));
            let flush = worker.flush();
            release.send(()).expect("fail write");
            let expected = if uncertain {
                Ok(CommitOutcome::DirectorySyncUncertain(io::ErrorKind::Other))
            } else {
                Err(JournalFailure::Io(io::ErrorKind::Other))
            };
            for receipt in [first, pending, flush] {
                assert_eq!(receive(receipt), expected);
            }
            assert_eq!(receive(worker.flush()), expected);
            assert_eq!(
                receive(worker.submit(snapshot("explicit retry"))),
                Ok(CommitOutcome::Durable)
            );
            assert_eq!(receive(worker.finish()), Ok(CommitOutcome::Durable));
        }
    }

    #[test]
    fn waiter_capacity_rejects_without_replacing_pending_snapshot() {
        let (entered, entries) = mpsc::channel();
        let (release, permits) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with(
            || Ok(((), LoadedJournal::Missing)),
            move |_, snapshot| {
                entered
                    .send(snapshot.drafts[0].1.text.clone())
                    .expect("entry");
                permits.recv().expect("release");
                Ok(JournalCommit::Durable)
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let first = worker.submit(snapshot("first"));
        entries.recv().expect("first entered");
        let mut receipts = vec![first, worker.submit(snapshot("retained"))];
        for _ in 2..MAX_WAITERS {
            receipts.push(worker.flush());
        }
        assert_eq!(
            receive(worker.submit(snapshot("rejected"))),
            Err(JournalFailure::AtCapacity)
        );
        assert_eq!(receive(worker.flush()), Err(JournalFailure::AtCapacity));
        let finish = worker.finish();
        release.send(()).expect("release first");
        assert_eq!(entries.recv().expect("retained pending"), "retained");
        release.send(()).expect("release pending");
        for receipt in receipts {
            assert_eq!(receive(receipt), Ok(CommitOutcome::Durable));
        }
        assert_eq!(receive(finish), Ok(CommitOutcome::Durable));
    }

    #[test]
    fn dropping_handle_drains_pending_work_and_releases_writer() {
        struct Writer(mpsc::Sender<()>);
        impl Drop for Writer {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (dropped, drop_notice) = mpsc::channel();
        let (entered, entries) = mpsc::channel();
        let (release, permits) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with(
            move || Ok((Writer(dropped), LoadedJournal::Missing)),
            move |_, _| {
                entered.send(()).expect("entered");
                permits.recv().expect("release");
                Ok(JournalCommit::Durable)
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let first = worker.submit(snapshot("first"));
        entries.recv().expect("first entered");
        let second = worker.submit(snapshot("second"));
        drop(worker);
        release.send(()).expect("release first");
        entries.recv().expect("second entered");
        release.send(()).expect("release second");
        assert_eq!(receive(first), Ok(CommitOutcome::Durable));
        assert_eq!(receive(second), Ok(CommitOutcome::Durable));
        drop_notice
            .recv()
            .expect("writer dropped after pending commit");
    }

    #[test]
    fn finish_receipt_releases_real_journal_lock() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = PathBuf::from(format!(
            "/tmp/opencode/journal-worker-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let path = directory.join("journal.json");
        let (worker, hydration) = JournalWorker::start(path.clone());
        assert!(matches!(
            hydration.blocking_recv(),
            Ok(Ok(LoadedJournal::Missing))
        ));
        assert!(matches!(
            MutationJournal::open(&path),
            Err(JournalError::Locked)
        ));
        assert_eq!(
            receive(worker.submit(snapshot("persisted"))),
            Ok(CommitOutcome::Durable)
        );
        assert_eq!(receive(worker.finish()), Ok(CommitOutcome::Durable));
        let (journal, loaded) = MutationJournal::open(&path).expect("finish released lock");
        let LoadedJournal::Present(snapshot) = loaded else {
            panic!("saved journal exists");
        };
        assert_eq!(snapshot.drafts[0].1.text, "persisted");
        drop(journal);
        std::fs::remove_dir_all(directory).expect("remove test journal");
    }

    #[test]
    fn flush_waits_for_inflight_work_and_finish_preserves_its_failure() {
        let (entered, entries) = mpsc::channel();
        let (release, permits) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with(
            || Ok(((), LoadedJournal::Missing)),
            move |_, _| {
                entered.send(()).expect("write entered");
                permits.recv().expect("release write");
                Err(JournalError::Invalid("invalid snapshot"))
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        let write = worker.submit(snapshot("first"));
        entries.recv().expect("write started");
        let mut flush = worker.flush();
        assert!(matches!(
            flush.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let finish = worker.finish();
        release.send(()).expect("release failing write");
        for receipt in [write, flush, finish] {
            assert_eq!(
                receive(receipt),
                Err(JournalFailure::Invalid("invalid snapshot"))
            );
        }
    }

    #[test]
    fn finish_during_open_waits_until_writer_is_released() {
        struct Writer(mpsc::Sender<()>);
        impl Drop for Writer {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (release, permit) = mpsc::channel();
        let (dropped, drop_notice) = mpsc::channel();
        let (worker, hydration) = JournalWorker::start_with(
            move || {
                permit.recv().expect("release open");
                Ok((Writer(dropped), LoadedJournal::Missing))
            },
            |_, _| panic!("no snapshots were accepted"),
        );
        let mut finish = worker.finish();
        assert!(matches!(
            finish.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        release.send(()).expect("release open");
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        assert_eq!(receive(finish), Ok(CommitOutcome::Durable));
        drop_notice
            .try_recv()
            .expect("writer dropped before finish receipt");
    }

    #[test]
    fn panicked_writer_rejects_retry_after_another_writer_updates_disk() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "journal-panic-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let path = directory.join("journal.json");
        let worker_path = path.clone();
        let (worker, hydration) = JournalWorker::start_with(
            move || MutationJournal::open(&worker_path),
            |_, _| panic!("injected worker failure"),
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        assert_eq!(
            receive(worker.submit(snapshot("unsaved first writer"))),
            Err(JournalFailure::WorkerStopped)
        );
        let (mut second, _) = MutationJournal::open(&path).expect("panic releases the lock");
        assert!(matches!(
            second.save(&snapshot("second writer")),
            Ok(JournalCommit::Durable)
        ));
        assert_eq!(
            receive(worker.submit(snapshot("stale retry"))),
            Err(JournalFailure::WorkerStopped)
        );
        drop(second);
        let (journal, loaded) = MutationJournal::open(&path).expect("read second writer's data");
        let LoadedJournal::Present(snapshot) = loaded else {
            panic!("second writer saved a snapshot");
        };
        assert_eq!(snapshot.drafts[0].1.text, "second writer");
        drop(journal);
        std::fs::remove_dir_all(directory).expect("remove test journal");
    }

    #[test]
    fn open_and_save_run_on_the_worker_thread() {
        let caller = thread::current().id();
        let (worker, hydration) = JournalWorker::start_with(
            move || {
                assert_ne!(
                    thread::current().id(),
                    caller,
                    "open must not perform caller-thread I/O"
                );
                Ok((thread::current().id(), LoadedJournal::Missing))
            },
            move |owner, _| {
                assert_ne!(
                    thread::current().id(),
                    caller,
                    "save must not perform caller-thread I/O"
                );
                assert_eq!(thread::current().id(), *owner, "one thread owns the writer");
                Ok(JournalCommit::Durable)
            },
        );
        assert!(matches!(hydration.blocking_recv(), Ok(Ok(_))));
        assert_eq!(
            receive(worker.submit(snapshot("text"))),
            Ok(CommitOutcome::Durable)
        );
        assert_eq!(receive(worker.finish()), Ok(CommitOutcome::Durable));
    }
}
