//! Support for real Workspace close consumers. No native lifecycle is simulated here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext};

use crate::journal_worker::{CommitOutcome, JournalResult, ReplyGate};
use crate::workspace::Workspace;

pub(super) fn finish_gate(
    cx: &TestAppContext,
    workspace: &Entity<Workspace>,
) -> ReplyGate<JournalResult> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .attachment_worker()
            .expect("real journal owner")
            .hold_finish_reply_for_test()
    })
}

pub(super) fn request_close(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
) {
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| workspace.request_close(window, cx));
    })
    .expect("normal close starts through the live Workspace");
}

pub(super) async fn assert_live_journal_wait(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    finish: &ReplyGate<JournalResult>,
) {
    assert!(
        cx.update_window(window, |_, _, _| ()).is_ok(),
        "accepted work retains its window"
    );
    let checkpoint = cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert!(
            workspace.close_in_progress(),
            "normal close has an active admission barrier"
        );
        assert!(
            workspace.persistence_ready(),
            "accepted callbacks still have a writable journal"
        );
        assert!(
            !workspace.persistence_closing(),
            "accepted work precedes final journal sealing"
        );
        workspace
            .attachment_worker()
            .expect("writer remains owned")
            .flush()
    });
    assert_eq!(
        checkpoint
            .await
            .expect("live worker answers the checkpoint"),
        Ok(CommitOutcome::Durable)
    );
    cx.run_until_parked();
    assert!(
        !finish.has_reached(),
        "a real worker checkpoint cannot finish ahead of held accepted work"
    );
}

pub(super) fn close_after_finish(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    finish: &ReplyGate<JournalResult>,
) {
    assert!(
        cx.update_window(window, |_, _, _| ()).is_ok(),
        "held real finish receipt retains its window"
    );
    finish.release();
    super::wait(cx, |cx| cx.update_window(window, |_, _, _| ()).is_err());
}

enum Target {
    Exact(String),
    SessionLookup,
    Suffix(String),
}

/// Only the fixture HTTP reply is paused. The native request and callback stay real.
pub(super) struct HeldResponse {
    method: String,
    target: Target,
    entered: Arc<AtomicBool>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
}

pub(super) struct ResponseHold {
    entered: Arc<AtomicBool>,
    release: Option<mpsc::Sender<()>>,
}

impl ResponseHold {
    pub(super) fn exact(method: &str, path: &str) -> (Self, HeldResponse) {
        Self::new(method, Target::Exact(path.to_owned()))
    }

    pub(super) fn session_lookup() -> (Self, HeldResponse) {
        Self::new("GET", Target::SessionLookup)
    }

    pub(super) fn suffix(method: &str, suffix: &str) -> (Self, HeldResponse) {
        Self::new(method, Target::Suffix(suffix.to_owned()))
    }

    fn new(method: &str, target: Target) -> (Self, HeldResponse) {
        let (release, waiting) = mpsc::channel();
        let entered = Arc::new(AtomicBool::new(false));
        (
            Self {
                entered: Arc::clone(&entered),
                release: Some(release),
            },
            HeldResponse {
                method: method.to_owned(),
                target,
                entered,
                release: Mutex::new(Some(waiting)),
            },
        )
    }

    pub(super) fn has_reached(&self) -> bool {
        self.entered.load(Ordering::Acquire)
    }

    pub(super) fn release(mut self) {
        self.release
            .take()
            .expect("one release sender")
            .send(())
            .expect("held fixture reply is live");
    }
}

impl HeldResponse {
    pub(super) fn wait(&self, method: &str, path: &str) {
        let matches = self.method == method
            && match &self.target {
                Target::Exact(expected) => path == expected,
                Target::Suffix(suffix) => path.ends_with(suffix),
                Target::SessionLookup => path
                    .strip_prefix("/api/session/")
                    .is_some_and(|id| !id.is_empty() && !id.contains('/') && !id.contains('?')),
            };
        if !matches {
            return;
        }
        let receiver = self
            .release
            .lock()
            .expect("reply gate is not poisoned")
            .take();
        let Some(receiver) = receiver else {
            return;
        };
        self.entered.store(true, Ordering::Release);
        match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("test must release its exact held HTTP reply")
            }
        }
    }
}
