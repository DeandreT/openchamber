//! Exact-ID observations and explicit retries of retained prompts. A check
//! reads every step of one attempt (its context items and its prompt); a retry
//! resends only the steps that check did not find, in their original order.

use std::collections::HashMap;

use gpui_kit::{Context, Task, Window};
use openchamber_native_core::draft::DraftTarget;
use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::pending_requests::RequestKey;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::prompt::SendObservation;

use crate::connection::ConnectionState;
use crate::recovery_view::{RecoveryPhase, RecoveryRow};
use crate::send_attempt::{Admission, SendOrigin, SessionKey};
use crate::server_sync::apply_list_change;
use crate::workspace::Workspace;

#[derive(Clone)]
enum CheckState {
    Checking,
    /// The prompt is missing. A retry observes again before resending.
    NotObserved,
    /// The read failed; checking again may succeed.
    Failed,
    /// A context item follows a missing one, so no resend keeps their order.
    OutOfOrder,
    /// The server definitely refused a retry of this exact request.
    Refused,
}

impl CheckState {
    /// No check or retry can settle the attempt; only the user can, by
    /// moving its prompt back to the composer.
    fn unresolvable(&self) -> bool {
        matches!(self, Self::OutOfOrder | Self::Refused)
    }
}

enum CheckAction {
    Observe,
    RetryIfMissing(Vec<RequestKey>),
}

struct Check {
    state: CheckState,
    since: u64,
}

/// Checks and their tasks per attempt: a session can retain several.
#[derive(Default)]
pub(crate) struct RecoveryState {
    checks: HashMap<(SessionKey, MessageId), Check>,
    tasks: HashMap<(SessionKey, MessageId), Task<()>>,
}

impl RecoveryState {
    pub(crate) fn clear_finished(&mut self, key: &SessionKey, id: &MessageId) {
        let attempt = (key.clone(), id.clone());
        self.checks.remove(&attempt);
        self.tasks.remove(&attempt);
    }
}

impl Workspace {
    pub(crate) fn recovery_items(&self, _cx: &gpui_kit::App) -> Vec<RecoveryRow> {
        let mut rows: Vec<_> = self
            .send_attempts
            .iter()
            .filter(|(key, _)| key.runtime == self.runtime)
            .map(|(key, attempt)| {
                let id = &attempt.prompt.message_id;
                let checking = self.recovery.checks.get(&(key.clone(), id.clone()));
                let created = attempt.origin != SendOrigin::Existing;
                let phase = if attempt.origin == SendOrigin::Deleted {
                    RecoveryPhase::CreateDeleted
                } else if let Some(check) = self.creations.checks.get(key).filter(|_| created) {
                    match check {
                        crate::create_attempt::CreationCheck::Checking => RecoveryPhase::Checking,
                        crate::create_attempt::CreationCheck::Mismatch => {
                            RecoveryPhase::CreateMismatch
                        }
                        _ => RecoveryPhase::CreateCheckFailed,
                    }
                } else if matches!(
                    checking.map(|check| &check.state),
                    Some(CheckState::Checking)
                ) {
                    RecoveryPhase::Checking
                } else if self.send_attempts.is_sending(key, id)
                    && attempt.admission == Admission::Uncertain
                {
                    RecoveryPhase::Sending
                } else {
                    match attempt.admission {
                        Admission::Preparing => RecoveryPhase::Preparing,
                        Admission::Sending { .. } => RecoveryPhase::Sending,
                        Admission::Admitted => RecoveryPhase::Queued,
                        Admission::Uncertain => match checking.map(|check| &check.state) {
                            Some(CheckState::Checking) => RecoveryPhase::Checking,
                            Some(CheckState::NotObserved) => RecoveryPhase::NotObserved,
                            Some(CheckState::Failed) => RecoveryPhase::CheckFailed,
                            Some(CheckState::OutOfOrder) => RecoveryPhase::OutOfOrder,
                            Some(CheckState::Refused) => RecoveryPhase::Refused,
                            None => RecoveryPhase::Uncertain,
                        },
                    }
                };
                let connected = self.connection.as_ref().is_some_and(|connection| {
                    matches!(connection.state, ConnectionState::Connected)
                });
                let busy = self.send_attempts.is_active(key)
                    || self.recovery.tasks.contains_key(&(key.clone(), id.clone()))
                    || self.creation_tasks.contains_key(key)
                    || attempt.origin == SendOrigin::Deleted;
                let can_restore = !busy
                    && self.persistence_ready()
                    && attempt.admission == Admission::Uncertain
                    && checking.is_some_and(|check| check.state.unresolvable());
                RecoveryRow {
                    key: key.clone(),
                    message: id.clone(),
                    can_restore,
                    session_label: self
                        .sessions
                        .get(&key.session)
                        .map(|session| session.title.clone().into())
                        .unwrap_or_else(|| key.session.as_str().to_owned().into()),
                    text: attempt
                        .prompt
                        .text
                        .chars()
                        .take(240)
                        .collect::<String>()
                        .into(),
                    phase,
                    can_check: connected
                        && !busy
                        && !self.persistence_closing()
                        && attempt.admission != Admission::Preparing,
                    can_retry: connected
                        && self.prompt_allowed(key)
                        && !busy
                        && self.persistence_ready()
                        && attempt.admission == Admission::Uncertain
                        && !(created && self.creations.checks.contains_key(key))
                        && matches!(
                            checking.map(|check| &check.state),
                            Some(CheckState::NotObserved)
                        ),
                }
            })
            .collect();
        rows.extend(self.creation_recovery_items());
        rows.sort_by(|a, b| {
            (a.key.session.as_str(), a.message.as_str())
                .cmp(&(b.key.session.as_str(), b.message.as_str()))
        });
        rows
    }

    /// The message of the first recovery row for `key`, as a click on it sends.
    #[cfg(test)]
    pub(crate) fn recovery_message(&self, key: &SessionKey, cx: &gpui_kit::App) -> MessageId {
        self.recovery_items(cx)
            .into_iter()
            .find(|row| &row.key == key)
            .expect("a recovery row for the session")
            .message
    }

    pub(crate) fn check_recovery(
        &mut self,
        key: &SessionKey,
        id: &MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.creations.get(key).is_some() {
            self.check_creation(key, false, window, cx);
            return;
        }
        if self
            .send_attempts
            .attempt(key, id)
            .is_some_and(|attempt| attempt.origin == SendOrigin::Created)
        {
            self.check_created_send(key, false, window, cx);
            return;
        }
        self.start_recovery_check(key, id, CheckAction::Observe, window, cx);
    }

    pub(crate) fn retry_recovery(
        &mut self,
        key: &SessionKey,
        id: &MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.creations.get(key).is_some() {
            self.check_creation(key, true, window, cx);
            return;
        }
        if self
            .send_attempts
            .attempt(key, id)
            .is_some_and(|attempt| attempt.origin == SendOrigin::Created)
        {
            self.check_created_send(key, true, window, cx);
            return;
        }
        if self.persistence_ready() {
            let requests = self.requests_for_send(key);
            self.start_recovery_check(key, id, CheckAction::RetryIfMissing(requests), window, cx);
        }
    }

    pub(crate) fn mark_refused(&mut self, key: &SessionKey, id: &MessageId) {
        self.recovery.checks.insert(
            (key.clone(), id.clone()),
            Check {
                state: CheckState::Refused,
                since: 0,
            },
        );
    }

    /// Moves an attempt no check or retry can settle back to its original
    /// draft, after any newer text, and removes it from the store. One journal
    /// snapshot holds both changes, so a restart shows either the attempt or
    /// the restored draft, never both and never neither.
    pub(crate) fn restore_recovery(
        &mut self,
        key: &SessionKey,
        id: &MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let attempt_key = (key.clone(), id.clone());
        if key.runtime != self.runtime
            || !self.persistence_ready()
            || self.recovery.tasks.contains_key(&attempt_key)
            || self.creation_tasks.contains_key(key)
            || !self
                .recovery
                .checks
                .get(&attempt_key)
                .is_some_and(|check| check.state.unresolvable())
        {
            return;
        }
        let Some(attempt) = self.send_attempts.discard(key, id) else {
            return;
        };
        self.recovery.clear_finished(key, id);
        if self.send_attempts.created_attempt(key).is_none() {
            self.creations.checks.remove(key);
        }
        self.return_to_draft(attempt, window, cx);
        self.start_background_save(cx);
        cx.notify();
    }

    /// Drops what this client holds for a session the server deleted, and for
    /// its descendants, which go with it: every send attempt and its checks,
    /// the creation and its checks, and the drafts, in one journal save. The
    /// web client clears the same state on a confirmed deletion. A chain still
    /// running for one of them finds its attempt gone and changes nothing.
    pub(crate) fn forget_deleted_session(&mut self, deleted: &SessionId, cx: &mut Context<Self>) {
        let mut sessions = vec![deleted.clone()];
        let records: Vec<_> = self
            .sessions
            .active()
            .iter()
            .chain(self.sessions.archived())
            .collect();
        let mut next = 0;
        while let Some(parent) = sessions.get(next).cloned() {
            for record in &records {
                if record.parent_id.as_ref() == Some(&parent) && !sessions.contains(&record.id) {
                    sessions.push(record.id.clone());
                }
            }
            next += 1;
        }
        let mut changed = false;
        for session in &sessions {
            let key = self.mutation_key(session);
            self.permission_modes.creating.remove(&key);
            changed |= !self.send_attempts.forget_session(&key).is_empty();
            self.recovery.checks.retain(|(owner, _), _| *owner != key);
            self.recovery.tasks.retain(|(owner, _), _| *owner != key);
            changed |= self.creations.remove(&key).is_some();
            changed |= self.creations.checks.remove(&key).is_some();
            let target = DraftTarget::Session(session.clone());
            let owners: Vec<_> = self
                .drafts
                .iter()
                .map(|(owner, _)| owner)
                .filter(|owner| *owner.runtime() == self.runtime && *owner.target() == target)
                .cloned()
                .collect();
            for owner in owners {
                self.drafts.take(&owner);
                self.draft_previews.remove(&owner);
                changed = true;
            }
            // The composer may still show its text; leaving the session must
            // not save it back.
            if self
                .composer_key
                .as_ref()
                .is_some_and(|owner| *owner.runtime() == self.runtime && *owner.target() == target)
            {
                self.composer_key = None;
                changed = true;
            }
        }
        if changed {
            self.start_background_save(cx);
            cx.notify();
        }
    }

    /// Forgets recovery state for the attempts in `before` that exact
    /// authority has since resolved.
    pub(crate) fn forget_resolved(
        &mut self,
        key: &SessionKey,
        before: &[MessageId],
        cx: &mut Context<Self>,
    ) {
        let resolved: Vec<_> = before
            .iter()
            .filter(|id| self.send_attempts.attempt(key, id).is_none())
            .cloned()
            .collect();
        if resolved.is_empty() {
            return;
        }
        for id in &resolved {
            self.recovery.clear_finished(key, id);
        }
        if self.send_attempts.created_attempt(key).is_none() {
            self.creations.checks.remove(key);
        }
        self.queue_persistence(cx);
    }

    pub(crate) fn continue_created_recovery(
        &mut self,
        key: &SessionKey,
        requests: Option<Vec<RequestKey>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self
            .send_attempts
            .created_attempt(key)
            .map(|attempt| attempt.prompt.message_id.clone())
        else {
            return;
        };
        self.start_recovery_check(
            key,
            &id,
            requests.map_or(CheckAction::Observe, CheckAction::RetryIfMissing),
            window,
            cx,
        );
    }

    fn start_recovery_check(
        &mut self,
        key: &SessionKey,
        id: &MessageId,
        action: CheckAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let attempt_key = (key.clone(), id.clone());
        if self.persistence_closing()
            || key.runtime != self.runtime
            || self.send_attempts.is_active(key)
            || self.recovery.tasks.contains_key(&attempt_key)
            || self.creation_tasks.contains_key(key)
        {
            return;
        }
        let Some(attempt) = self.send_attempts.attempt(key, id) else {
            return;
        };
        if attempt.admission == Admission::Preparing || attempt.origin == SendOrigin::Deleted {
            return;
        }
        let prompt = attempt.prompt.clone();
        let id = id.clone();
        let Some(connection) = self
            .connection
            .as_ref()
            .filter(|connection| matches!(connection.state, ConnectionState::Connected))
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let session = key.session.clone();
        let since = self
            .open_sessions
            .get(&key.session)
            .map_or(0, |open| open.transcript.revision());
        let request = connection
            .network
            .run(async move { client.observe_send(&session, &prompt).await });
        self.recovery.checks.insert(
            attempt_key.clone(),
            Check {
                state: CheckState::Checking,
                since,
            },
        );
        let key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = request.await.unwrap_or(Err(TransportError::Cancelled));
            let _ = this.update_in(cx, |this, window, cx| {
                this.recovery.tasks.remove(&(key.clone(), id.clone()));
                this.finish_recovery_check(&key, &id, action, result, window, cx);
            });
        });
        self.recovery.tasks.insert(attempt_key, task);
        cx.notify();
    }

    fn finish_recovery_check(
        &mut self,
        key: &SessionKey,
        id: &MessageId,
        action: CheckAction,
        result: Result<SendObservation, TransportError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let attempt_key = (key.clone(), id.clone());
        let Some(check) = self.recovery.checks.get(&attempt_key) else {
            return;
        };
        let since = check.since;
        if self.send_attempts.attempt(key, id).is_none() {
            self.recovery.checks.remove(&attempt_key);
            return;
        }
        let failed = Check {
            state: CheckState::Failed,
            since,
        };
        match result {
            Ok(SendObservation::Pending(_)) => {
                self.send_attempts.confirm_pending(key, id);
                self.recovery.checks.remove(&attempt_key);
                self.queue_persistence(cx);
            }
            Ok(SendObservation::Delivered(message)) => {
                if key.runtime == self.runtime {
                    if let Some(open) = self.open_sessions.get_mut(&key.session) {
                        let change = open.transcript.apply_message_snapshot(*message, since);
                        apply_list_change(open, &change, cx);
                    }
                    self.load_history(&key.session, cx);
                }
                self.send_attempts.observe(key, id);
                self.recovery.checks.remove(&attempt_key);
                self.queue_persistence(cx);
            }
            Ok(SendObservation::Missing { admitted }) => {
                self.recovery.checks.insert(
                    attempt_key,
                    Check {
                        state: CheckState::NotObserved,
                        since,
                    },
                );
                if let CheckAction::RetryIfMissing(requests) = action
                    && key.runtime == self.runtime
                {
                    self.retry_persisted_prompt(key, id, requests, admitted, window, cx);
                }
            }
            // A context step found after a missing one cannot be resent in
            // order, so the attempt stays uncertain without a retry.
            Ok(SendObservation::OutOfOrder) => {
                self.recovery.checks.insert(
                    attempt_key,
                    Check {
                        state: CheckState::OutOfOrder,
                        since,
                    },
                );
            }
            Err(_) => {
                self.recovery.checks.insert(attempt_key, failed);
            }
        }
        cx.notify();
    }
}
