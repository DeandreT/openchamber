//! The composer's permission-mode control: the server's per-session policy,
//! a new-session draft's own choice, and which fresh permission requests stay
//! out of sight while the server decides them.
//!
//! Mirrors `stores/permissionStore.ts`, `components/chat/permissionAutoAccept.ts`
//! and the hold-back in `sync/sync-context.tsx`. The server's
//! `permission-auto-accept` runtime is the only responder; this module never
//! answers a request.

use std::collections::HashMap;

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use gpui_kit::{Context, Task, Window};
use openchamber_native_core::ids::{PermissionId, RuntimeId, SessionId};
use openchamber_native_core::pending_requests::RequestId;
use openchamber_native_core::permission_mode::{
    AutoAnswerHolds, LeftForUser, PermissionMode, PermissionPolicy, PolicySnapshot,
    is_space_directory,
};
use openchamber_native_core::request_owner::RequestOwner;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::error::TransportError;

use crate::locale;
use crate::network::NetworkTask;
use crate::send_attempt::SessionKey;
use crate::workspace::Workspace;

/// Identifies one read or write so its completion can be checked against
/// the state it started in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Operation {
    /// Bumped when the runtime changes; older completions are dropped.
    generation: u64,
    sequence: u64,
}

/// One mode write, captured when the user (or a creation handoff) asks.
struct ModeWrite {
    key: SessionKey,
    mode: PermissionMode,
    directory: Option<String>,
    /// Whether a failure is shown to the user.
    report: bool,
}

#[derive(Default)]
pub(crate) struct PermissionModes {
    pub(crate) policy: PermissionPolicy,
    generation: u64,
    sequence: u64,
    latest_started: u64,
    /// The connection generation the last policy read was started for.
    read_connection: Option<u64>,
    read: Option<Task<()>>,
    /// Writes keep running across a runtime switch; their completion is
    /// dropped there.
    writes: HashMap<u64, Task<()>>,
    /// A new-session draft's explicit choice, per runtime.
    pub(crate) drafts: HashMap<RuntimeId, PermissionMode>,
    /// A submitted draft's choice, written once its session is verified.
    /// Kept in memory only: after a restart the server's default applies.
    pub(crate) creating: HashMap<SessionKey, PermissionMode>,
    pub(crate) holds: AutoAnswerHolds,
    /// What still has to happen before a reconnect's replay window closes.
    replay_gate: Option<ReplayGate>,
}

/// A stale reconnect's replay window stays open until the reconnect's
/// policy read settled and the replay's last event was applied. The read is
/// a separate request and can settle while the replay is still arriving, so
/// neither alone marks the end of the replay.
struct ReplayGate {
    read_settled: bool,
    /// The last replayed event still to be applied; `None` once applied, or
    /// when nothing was replayed.
    through: Option<String>,
}

impl PermissionModes {
    fn begin(&mut self) -> Operation {
        self.sequence += 1;
        self.latest_started = self.sequence;
        Operation {
            generation: self.generation,
            sequence: self.sequence,
        }
    }

    /// Applies an operation's snapshot unless the runtime changed since it
    /// started or a newer operation overtakes an unrevisioned answer.
    fn complete(&mut self, operation: Operation, snapshot: PolicySnapshot) -> bool {
        if operation.generation != self.generation
            || (snapshot.revision.is_none() && operation.sequence != self.latest_started)
        {
            return false;
        }
        self.policy.apply(snapshot)
    }

    /// Forgets the previous runtime's policy and holds. Draft choices keep
    /// their runtime keys; in-flight writes finish without applying.
    pub(crate) fn reset(&mut self) {
        self.generation += 1;
        self.latest_started = 0;
        self.policy = PermissionPolicy::default();
        self.read_connection = None;
        self.read = None;
        self.holds = AutoAnswerHolds::default();
        self.replay_gate = None;
    }

    /// Opens a reconnect's replay window, superseding any open one.
    /// `through` is the last replayed event, from a resumed replay.
    pub(crate) fn begin_replay(&mut self, through: Option<String>) {
        self.holds.open_replay_window();
        self.replay_gate = Some(ReplayGate {
            read_settled: false,
            through,
        });
    }

    /// The stream applied the event `id`.
    pub(crate) fn event_applied(&mut self, id: &str) {
        if let Some(gate) = &mut self.replay_gate
            && gate.through.as_deref() == Some(id)
        {
            gate.through = None;
            self.close_replay_if_done();
        }
    }

    fn policy_read_settled(&mut self) {
        if let Some(gate) = &mut self.replay_gate {
            gate.read_settled = true;
            self.close_replay_if_done();
        }
    }

    fn close_replay_if_done(&mut self) {
        if self
            .replay_gate
            .as_ref()
            .is_some_and(|gate| gate.read_settled && gate.through.is_none())
        {
            self.replay_gate = None;
            let replayed = self.holds.close_replay_window();
            if replayed > 0 {
                tracing::debug!(replayed, "requests held during a replay are shown");
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn writes_in_flight(&self) -> usize {
        self.writes.len()
    }

    #[cfg(test)]
    pub(crate) fn read_idle(&self) -> bool {
        self.read.is_none()
    }
}

impl Workspace {
    fn session_parent(&self, id: &SessionId) -> Option<SessionId> {
        self.sessions.get(id)?.parent_id.clone()
    }

    /// Whether a session lives in an isolated space. Its mode is in that
    /// space's policy, which this client does not read: the host's policy
    /// never decides it.
    fn session_in_space(&self, id: &SessionId) -> bool {
        self.sessions
            .get(id)
            .and_then(|record| record.resolved_directory())
            .is_some_and(|directory| is_space_directory(&directory))
    }

    /// The mode a session runs in: its nearest explicit ancestor's.
    pub(crate) fn session_permission_mode(&self, id: &SessionId) -> PermissionMode {
        self.permission_modes
            .policy
            .mode(id, |session| self.session_parent(session))
    }

    /// The mode the composer stands for: the selected session's, or the
    /// new-session draft's own choice, else the Settings default the server
    /// will write onto it.
    pub(crate) fn composer_permission_mode(&self) -> PermissionMode {
        match &self.selected {
            // Unknown: the shield shows the default and stays inert.
            Some(id) if self.session_in_space(id) => PermissionMode::Ask,
            Some(id) => self.session_permission_mode(id),
            None => self
                .permission_modes
                .drafts
                .get(&self.runtime)
                .copied()
                .or_else(|| {
                    self.send_catalog
                        .settings
                        .as_ref()
                        .and_then(|settings| settings.permission_default_mode)
                })
                .unwrap_or_default(),
        }
    }

    /// What the shield shows: a stored Safety reads as Ask while the safety
    /// net cannot run.
    pub(crate) fn shown_permission_mode(&self) -> PermissionMode {
        self.composer_permission_mode()
            .displayed(self.send_catalog.safety_net)
    }

    /// Synthetic data has no server to keep a policy, and a space
    /// session's policy is not read here.
    pub(crate) fn permission_mode_interactive(&self) -> bool {
        !self.is_synthetic()
            && self
                .selected
                .as_ref()
                .is_none_or(|id| !self.session_in_space(id))
    }

    /// One press of the shield: the next mode, on the draft or the session.
    pub(crate) fn cycle_permission_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.permission_mode_interactive() {
            return;
        }
        let next = self
            .composer_permission_mode()
            .next(self.send_catalog.safety_net);
        match self.selected.clone() {
            None => {
                self.permission_modes
                    .drafts
                    .insert(self.runtime.clone(), next);
                cx.notify();
            }
            Some(session) => {
                let Some((client, executor)) = self.connection.as_ref().and_then(|connection| {
                    Some((connection.client.clone()?, connection.network.handle()))
                }) else {
                    self.notify_permission_toggle_failed(window, cx);
                    return;
                };
                let directory = self
                    .sessions
                    .get(&session)
                    .and_then(|record| record.resolved_directory());
                let key = SessionKey {
                    runtime: self.runtime.clone(),
                    session,
                };
                let write = ModeWrite {
                    key,
                    mode: next,
                    directory,
                    report: true,
                };
                self.write_permission_mode(write, client, &executor, window, cx);
            }
        }
    }

    /// Writes the choice a submitted draft carried onto its verified
    /// session, beside the first send and without waiting for it. A failure
    /// is logged; the session then shows the server's policy.
    pub(crate) fn write_created_permission_mode(
        &mut self,
        key: &SessionKey,
        directory: &str,
        client: ServerClient,
        executor: &tokio::runtime::Handle,
        context: (&mut Window, &mut Context<Self>),
    ) {
        let Some(mode) = self.permission_modes.creating.remove(key) else {
            return;
        };
        let (window, cx) = context;
        let write = ModeWrite {
            key: key.clone(),
            mode,
            directory: Some(directory.to_owned()),
            report: false,
        };
        self.write_permission_mode(write, client, executor, window, cx);
    }

    /// Writes a session's mode with the client of the session's runtime and
    /// applies the policy the server answers with, while that runtime is
    /// still shown. A failure keeps the last policy.
    fn write_permission_mode(
        &mut self,
        write: ModeWrite,
        client: ServerClient,
        executor: &tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ModeWrite {
            key,
            mode,
            directory,
            report,
        } = write;
        let space = directory.as_deref().is_some_and(is_space_directory);
        let operation = self.permission_modes.begin();
        let request = NetworkTask::spawn_on(executor, async move {
            client
                .set_permission_mode(&key.session, mode, directory.as_deref())
                .await
        });
        let runtime = key.runtime;
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = request.await.unwrap_or(Err(TransportError::Cancelled));
            let _ = this.update_in(cx, |this, window, cx| {
                this.permission_modes.writes.remove(&operation.sequence);
                if this.runtime != runtime
                    || operation.generation != this.permission_modes.generation
                {
                    return;
                }
                match result {
                    // A space answers with its own policy, not the host's.
                    Ok(_) if space => {}
                    Ok(snapshot) => {
                        if this.permission_modes.complete(operation, snapshot) {
                            this.release_unanswered_holds();
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the permission mode could not be saved");
                        if report {
                            this.notify_permission_toggle_failed(window, cx);
                        }
                    }
                }
                cx.notify();
            });
        });
        self.permission_modes
            .writes
            .insert(operation.sequence, task);
    }

    fn notify_permission_toggle_failed(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.push_notification(
            Notification::error(
                locale::strings(cx).get("chat.chatInput.toast.togglePermissionAutoAcceptFailed"),
            ),
            cx,
        );
    }

    /// Reads the host's policy for the current connection. A failed read
    /// keeps the last valid policy.
    pub(crate) fn load_permission_policy(&mut self, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        self.permission_modes.read_connection = Some(connection.generation);
        let operation = self.permission_modes.begin();
        let read = connection
            .network
            .run(async move { client.permission_policy().await });
        self.permission_modes.read = Some(cx.spawn(async move |this, cx| {
            let Some(result) = read.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if operation.generation != this.permission_modes.generation {
                    return;
                }
                this.permission_modes.read = None;
                match result {
                    Ok(snapshot) => {
                        if this.permission_modes.complete(operation, snapshot) {
                            this.release_unanswered_holds();
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the permission policy could not be read");
                    }
                }
                this.permission_modes.policy_read_settled();
                cx.notify();
            });
        }));
    }

    /// Starts a policy read once per connection, from the render path as
    /// routing readiness does.
    pub(crate) fn ensure_permission_policy(&mut self, cx: &mut Context<Self>) {
        if self.connection.as_ref().is_some_and(|connection| {
            self.permission_modes.read_connection != Some(connection.generation)
        }) {
            self.load_permission_policy(cx);
        }
    }

    /// A policy the server broadcast after a change.
    pub(crate) fn apply_permission_policy(&mut self, snapshot: PolicySnapshot) {
        if self.permission_modes.policy.apply(snapshot) {
            self.release_unanswered_holds();
        }
    }

    /// Shows the held requests of every session the server no longer
    /// answers without the user. A request asked just before a switch to
    /// Ask, or before the safety net stopped, is answered `ignored` with no
    /// `left-for-user`, so its hold would otherwise never end.
    pub(crate) fn release_unanswered_holds(&mut self) {
        let mut holds = std::mem::take(&mut self.permission_modes.holds);
        let released = holds.release_sessions(|session| {
            self.session_in_space(session)
                || !self
                    .session_permission_mode(session)
                    .answers_without_user(self.send_catalog.safety_net)
        });
        self.permission_modes.holds = holds;
        if released > 0 {
            tracing::debug!(
                released,
                "held permission requests are shown after a policy change"
            );
        }
    }

    /// Whether a fresh `permission.asked` stays out of sight until the server
    /// decides it. A request already on screen is never hidden again.
    pub(crate) fn hold_permission_request(
        &mut self,
        session: &SessionId,
        permission: &PermissionId,
        shown: bool,
    ) {
        if shown || self.session_in_space(session) {
            return;
        }
        let answered = self
            .session_permission_mode(session)
            .answers_without_user(self.send_catalog.safety_net);
        self.permission_modes
            .holds
            .asked(session, permission, answered);
    }

    /// The server left a request for the user: show it now, or as soon as
    /// it arrives.
    pub(crate) fn permission_left_for_user(
        &mut self,
        session: SessionId,
        permission: PermissionId,
    ) {
        let shown = self.blocking.stores.iter().any(|(scope, store)| {
            scope.runtime() == &self.runtime
                && store.entries().any(|(key, _)| {
                    key.owner == RequestOwner::Session(session.clone())
                        && key.id == RequestId::Permission(permission.clone())
                })
        });
        if self
            .permission_modes
            .holds
            .left_for_user(&session, &permission, shown)
            == LeftForUser::Revealed
        {
            tracing::debug!("a held permission request was left for the user");
        }
    }

    /// Whether a pending request is hidden while the server decides it.
    pub(crate) fn request_held(&self, owner: &RequestOwner, id: &RequestId) -> bool {
        match (owner, id) {
            (RequestOwner::Session(session), RequestId::Permission(permission)) => {
                self.permission_modes.holds.is_held(session, permission)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_from_a_previous_runtime_or_an_overtaken_read_are_dropped() {
        let snapshot = |mode, revision| PolicySnapshot {
            modes: [(SessionId::from("s"), mode)].into(),
            revision,
        };
        let mut modes = PermissionModes::default();
        let old = modes.begin();
        modes.reset();
        assert!(!modes.complete(old, snapshot(PermissionMode::Auto, Some(1))));
        assert!(!modes.policy.is_loaded());

        let first = modes.begin();
        let second = modes.begin();
        assert!(
            !modes.complete(first, snapshot(PermissionMode::Auto, None)),
            "an unrevisioned answer from an overtaken operation is dropped"
        );
        assert!(modes.complete(second, snapshot(PermissionMode::Safety, None)));
        assert!(
            modes.complete(first, snapshot(PermissionMode::Auto, Some(2))),
            "a revisioned answer is ordered by its revision"
        );
        assert!(!modes.complete(second, snapshot(PermissionMode::Ask, Some(1))));
        assert_eq!(
            modes.policy.mode(&SessionId::from("s"), |_| None),
            PermissionMode::Auto
        );
    }
}
