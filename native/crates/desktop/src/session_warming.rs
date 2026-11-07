//! The row menu's Keep warm and Stop warm, mirrored from the server's
//! per-session warming (`useSessionWarmingStore`, `session-warming-sync.ts`).
//!
//! The snapshot is read on every event-stream connection, since changes made
//! during a gap were not seen, and each broadcast carries a whole snapshot.
//! An older revision never replaces a newer one. A toggle waits for the
//! server's answer, which is the snapshot after the change.

use std::collections::{HashMap, HashSet};

use gpui_kit::{Context, Task, Window};
use openchamber_native_core::ids::SessionId;
use openchamber_native_transport::session_warming::WarmingSnapshot;

use crate::send_attempt::SessionKey;
use crate::workspace::Workspace;

/// The warming state the menu reads.
#[derive(Default)]
pub(crate) struct SessionWarming {
    /// `None` until the first snapshot of this runtime arrives.
    revision: Option<i64>,
    enabled: bool,
    sessions: HashMap<SessionId, Option<i64>>,
    changing: HashSet<SessionKey>,
    read: Option<Task<()>>,
}

impl SessionWarming {
    /// Settings chose per-session warming.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn is_warmed(&self, session: &SessionId) -> bool {
        self.sessions.contains_key(session)
    }

    pub(crate) fn is_changing(&self, key: &SessionKey) -> bool {
        self.changing.contains(key)
    }

    /// Applies a snapshot unless a newer one is already shown. Returns
    /// whether anything changed.
    pub(crate) fn apply(&mut self, snapshot: WarmingSnapshot) -> bool {
        if self
            .revision
            .is_some_and(|current| snapshot.revision < current)
        {
            return false;
        }
        self.revision = Some(snapshot.revision);
        self.enabled = snapshot.enabled;
        self.sessions = snapshot.sessions.into_iter().collect();
        true
    }

    /// Another runtime's sessions and revisions mean nothing here.
    pub(crate) fn switch_runtime(&mut self) {
        *self = Self {
            changing: std::mem::take(&mut self.changing),
            ..Self::default()
        };
    }
}

impl Workspace {
    /// Reads the warming snapshot again, as each event-stream connection
    /// does. A failed read keeps what is shown.
    pub(crate) fn load_session_warming(&mut self, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let runtime = self.runtime.clone();
        let read = connection
            .network
            .run(async move { client.session_warming().await });
        self.session_management.warming.read = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                if this.runtime != runtime {
                    return;
                }
                match result {
                    Some(Ok(snapshot)) => {
                        if this.session_management.warming.apply(snapshot) {
                            cx.notify();
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "the session warming state could not be read");
                    }
                    None => {}
                }
            });
        }));
    }

    /// A warming broadcast from the event stream.
    pub(crate) fn apply_session_warming(&mut self, snapshot: WarmingSnapshot) {
        self.session_management.warming.apply(snapshot);
    }

    /// Keep warm or Stop warm from the row menu.
    pub(crate) fn set_session_warm(
        &mut self,
        session: &SessionId,
        warm: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.mutation_key(session);
        if self.session_management.warming.changing.contains(&key) {
            return;
        }
        let Some(directory) = self
            .sessions
            .get(session)
            .and_then(|record| record.resolved_directory())
        else {
            return;
        };
        let id = session.clone();
        let finished = key.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move { client.set_session_warm(&id, &directory, warm).await })
            },
            move |this, result, window, cx| {
                this.session_management.warming.changing.remove(&finished);
                if finished.runtime != this.runtime {
                    return;
                }
                match result {
                    Some(Ok(snapshot)) => {
                        this.session_management.warming.apply(snapshot);
                    }
                    Some(Err(_)) | None => {
                        this.push_error("sessions.warming.toggleFailed", None, window, cx)
                    }
                }
            },
            window,
            cx,
        );
        if started {
            self.session_management.warming.changing.insert(key);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(revision: i64, enabled: bool, sessions: &[&str]) -> WarmingSnapshot {
        WarmingSnapshot {
            revision,
            enabled,
            sessions: sessions
                .iter()
                .map(|id| (SessionId::from(*id), None))
                .collect(),
        }
    }

    #[test]
    fn an_older_snapshot_never_replaces_a_newer_one() {
        let mut warming = SessionWarming::default();
        assert!(warming.apply(snapshot(2, true, &["a"])));
        assert!(warming.enabled() && warming.is_warmed(&SessionId::from("a")));
        assert!(
            !warming.apply(snapshot(1, false, &[])),
            "a late read is older"
        );
        assert!(warming.is_warmed(&SessionId::from("a")));
        assert!(warming.apply(snapshot(3, true, &[])));
        assert!(!warming.is_warmed(&SessionId::from("a")));
    }

    #[test]
    fn a_runtime_switch_forgets_the_snapshot() {
        let mut warming = SessionWarming::default();
        warming.apply(snapshot(9, true, &["a"]));
        warming.switch_runtime();
        assert!(!warming.enabled());
        assert!(
            warming.apply(snapshot(1, true, &[])),
            "the new runtime's revisions start over"
        );
    }
}
