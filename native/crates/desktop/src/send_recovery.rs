//! Exact-ID observations and explicit retries of retained prompts. A check
//! reads every step of one attempt (its context items and its prompt); a retry
//! resends only the steps that check did not find, in their original order.

use std::collections::HashMap;

use gpui_kit::{Context, Task, Window};
use openchamber_native_core::draft::DraftTarget;
use openchamber_native_core::ids::{MessageId, RuntimeId, SessionId};
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
    /// Queue request recovery uses operation identities, never prompt identities.
    /// Inspecting this projection performs no spool read or network request.
    pub(crate) fn enqueue_recovery_items(&self) -> Vec<crate::recovery_view::EnqueueRecoveryRow> {
        use crate::queue_storage::{QueueAction, QueueEnqueuePhase};

        let connected = self.connection.as_ref().is_some_and(|connection| {
            connection.state == ConnectionState::Connected && connection.client.is_some()
        });
        let mut rows: Vec<_> = self.queue_storage.operations().iter().filter_map(|operation| {
            if operation.owner().runtime() != self.runtime.as_str() { return None; }
            let QueueAction::Enqueue { phase, .. } = operation.action() else { return None; };
            let busy = self.queue.enqueues.busy(operation.id());
            let session = SessionId::from(operation.owner().session());
            let accepted_state = self.queue.enqueues.delivery(operation.id());
            let check = self.queue.enqueues.check(operation.id());
            let status = if check == crate::queue_enqueue::EnqueueCheck::Unknown
                && accepted_state == Some(openchamber_native_transport::queue_manifest::QueueEnqueueState::Queued) {
                crate::queue_enqueue::EnqueueCheck::Queued
            } else { check };
            Some(crate::recovery_view::EnqueueRecoveryRow {
                id: operation.id().clone(),
                session_label: self.sessions.get(&session).map_or_else(
                    || session.to_string().into(), |session| session.title.clone().into()),
                status,
                can_check: connected && !busy && self.persistence_ready() && !self.close_in_progress()
                    && !matches!(phase, QueueEnqueuePhase::Capturing),
                can_retry: connected && !busy && self.persistence_ready() && !self.close_in_progress()
                    && (!matches!(phase, QueueEnqueuePhase::Accepted { .. }) || accepted_state == Some(openchamber_native_transport::queue_manifest::QueueEnqueueState::Uncertain)),
            })
        }).collect();
        rows.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        rows
    }

    /// Retained transfers are explicit recovery actions, including after restart.
    /// Building these rows grants no byte, acknowledgement or dispatch authority.
    pub(crate) fn transfer_recovery_items(&self) -> Vec<crate::recovery_view::TransferRecoveryRow> {
        use crate::queue_local_send::LocalSendCheck;
        use crate::queue_prefix::PrefixCheck;
        use crate::queue_storage::{
            QueueAction, QueuePrefixPhase, QueueSendPhase, QueueTakePhase, QueueTakePurpose,
            QueueTakeSendState,
        };
        use crate::queue_take::TakeCheck;
        use crate::recovery_view::{TransferKind, TransferRecoveryRow};

        let connected = self.connection.as_ref().is_some_and(|connection| {
            connection.state == ConnectionState::Connected && connection.client.is_some()
        });
        let ready = connected && self.persistence_ready() && !self.close_in_progress();
        let mut rows = Vec::new();
        for operation in self.queue_storage.operations() {
            if operation.owner().runtime() != self.runtime.as_str() {
                continue;
            }
            let (kind, status, busy, can_check, can_retry) = match operation.action() {
                QueueAction::Take { purpose, phase, .. } => {
                    let busy = self.queue.takes.busy(operation.id());
                    if matches!(purpose, QueueTakePurpose::Edit)
                        && matches!(phase, QueueTakePhase::Acknowledged { .. })
                        && !busy
                    {
                        continue;
                    }
                    let status = match self.queue.takes.check(operation.id()) {
                        TakeCheck::Working => "chat.nativeRecovery.transferWorking",
                        TakeCheck::Failed => "chat.nativeRecovery.transferFailed",
                        TakeCheck::NotObserved | TakeCheck::Missing => {
                            "chat.nativeRecovery.notObserved"
                        }
                        TakeCheck::Admitted => "chat.nativeRecovery.queued",
                        TakeCheck::Unknown
                            if matches!(phase, QueueTakePhase::Acknowledged { local, .. }
                            if local.send == QueueTakeSendState::Admitted) =>
                        {
                            "chat.nativeRecovery.queued"
                        }
                        TakeCheck::Unknown | TakeCheck::LocalOwned => {
                            "chat.nativeRecovery.transferUnknown"
                        }
                    };
                    (TransferKind::Take, status, busy, true, true)
                }
                QueueAction::Prefix { phase, .. } => {
                    let busy = self.queue.prefixes.busy(operation.id());
                    let check = self.queue.prefixes.check(operation.id());
                    let status = match check {
                        PrefixCheck::Working => "chat.nativeRecovery.transferWorking",
                        PrefixCheck::Missing => "chat.nativeRecovery.notObserved",
                        PrefixCheck::Admitted => "chat.nativeRecovery.queued",
                        PrefixCheck::OutOfOrder => "chat.nativeRecovery.outOfOrder",
                        PrefixCheck::Failed => "chat.nativeRecovery.transferFailed",
                        PrefixCheck::Unknown => match phase {
                            QueuePrefixPhase::Admitted(_) => "chat.nativeRecovery.queued",
                            QueuePrefixPhase::Capturing => "chat.nativeRecovery.preparing",
                            _ => "chat.nativeRecovery.transferUnknown",
                        },
                    };
                    (
                        TransferKind::Prefix,
                        status,
                        busy,
                        !matches!(phase, QueuePrefixPhase::Capturing),
                        check != PrefixCheck::OutOfOrder,
                    )
                }
                QueueAction::Send {
                    phase: QueueSendPhase::Deleted,
                    ..
                } => continue,
                QueueAction::Send { phase, .. } => {
                    let busy = self.queue.local_sends.busy(operation.id());
                    let check = self.queue.local_sends.check(operation.id());
                    let status = match check {
                        LocalSendCheck::Working => "chat.nativeRecovery.transferWorking",
                        LocalSendCheck::Missing => "chat.nativeRecovery.notObserved",
                        LocalSendCheck::Admitted => "chat.nativeRecovery.queued",
                        LocalSendCheck::OutOfOrder => "chat.nativeRecovery.outOfOrder",
                        LocalSendCheck::PreparationUnavailable => "chat.nativeRecovery.sendFailed",
                        LocalSendCheck::Failed => "chat.nativeRecovery.transferFailed",
                        LocalSendCheck::Unknown => match phase {
                            QueueSendPhase::Capturing => "chat.nativeRecovery.preparing",
                            QueueSendPhase::Admitted { .. } => "chat.nativeRecovery.queued",
                            _ => "chat.nativeRecovery.transferUnknown",
                        },
                    };
                    (
                        TransferKind::LocalSend,
                        status,
                        busy,
                        !matches!(phase, QueueSendPhase::Capturing),
                        check != LocalSendCheck::OutOfOrder,
                    )
                }
                _ => continue,
            };
            let session = SessionId::from(operation.owner().session());
            rows.push(TransferRecoveryRow {
                id: operation.id().clone(),
                kind,
                status,
                session_label: self.sessions.get(&session).map_or_else(
                    || session.to_string().into(),
                    |session| session.title.clone().into(),
                ),
                can_check: ready && !busy && can_check,
                can_retry: ready
                    && !busy
                    && can_retry
                    && (kind == TransferKind::LocalSend
                        || !self.payload_local_send_blocks_session(&self.runtime, &session)),
            });
        }
        rows.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        rows
    }

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
                    && !attempt.queue_owned
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
                        && !self.payload_local_send_blocks_session(&key.runtime, &key.session)
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
        if self.payload_local_send_blocks_session(&key.runtime, &key.session) {
            return;
        }
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
            // A send that left open requests alone never cancels them on retry.
            let supersedes = self
                .send_attempts
                .attempt(key, id)
                .is_none_or(|attempt| attempt.supersedes);
            let requests = if supersedes {
                self.requests_for_send(key)
            } else {
                Vec::new()
            };
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
            || self
                .send_attempts
                .attempt(key, id)
                .is_some_and(|attempt| attempt.queue_owned)
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
        self.return_to_draft(key, attempt, window, cx);
        self.start_background_save(cx);
        cx.notify();
    }

    /// Capture the current runtime's subtree before its root leaves the index.
    pub(crate) fn deleted_session_subtree(&self, deleted: &SessionId) -> Vec<SessionId> {
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
        sessions
    }

    pub(crate) fn forget_deleted_session(
        &mut self,
        deleted: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sessions = self.deleted_session_subtree(deleted);
        self.forget_deleted_sessions(&self.runtime.clone(), &sessions, window, cx);
    }

    /// Explicit IDs belong to the runtime that confirmed deletion. A completion
    /// after a host switch never consults the replacement runtime's session index.
    pub(crate) fn forget_deleted_sessions(
        &mut self,
        runtime: &RuntimeId,
        sessions: &[SessionId],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if sessions.is_empty() {
            return;
        }
        let operations = match self.change_queue_storage(|queue, workspace| {
            queue.delete_local_sends(runtime, sessions, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        }) {
            Ok(operations) => operations,
            Err(_) => {
                // Original drafts and ledger roots remain authoritative on refusal.
                // Cancel HTTP actors without discarding any durable byte obligation.
                let roots = self
                    .queue
                    .local_sends
                    .forget_deleted_sessions(runtime, sessions, &[]);
                self.retire_released_native_roots(roots, window, cx);
                self.notify_mutation_failure("chat.nativeRecovery.storageFailed", window, cx);
                self.resume_native_root_close(window, cx);
                return;
            }
        };
        let mut released =
            self.queue
                .local_sends
                .forget_deleted_sessions(runtime, sessions, &operations);
        released.extend(
            self.composer_documents
                .forget_deleted_sessions(runtime, sessions),
        );
        for session in sessions {
            let key = SessionKey {
                runtime: runtime.clone(),
                session: session.clone(),
            };
            self.permission_modes.creating.remove(&key);
            self.send_attempts.forget_session(&key);
            self.recovery.checks.retain(|(owner, _), _| *owner != key);
            self.recovery.tasks.retain(|(owner, _), _| *owner != key);
            self.creations.remove(&key);
            self.creations.checks.remove(&key);
            let target = DraftTarget::Session(session.clone());
            let owners: Vec<_> = self
                .drafts
                .iter()
                .map(|(owner, _)| owner)
                .filter(|owner| owner.runtime() == runtime && *owner.target() == target)
                .cloned()
                .collect();
            for owner in owners {
                if let Some(draft) = self.drafts.take(&owner) {
                    released.extend(
                        draft.content.files.into_iter().flat_map(|file| {
                            std::iter::once(file.id).chain(file.source_document_id)
                        }),
                    );
                }
                self.draft_previews.remove(&owner);
            }
            // The composer may still show its text; leaving the session must
            // not save it back.
            if self
                .composer_key
                .as_ref()
                .is_some_and(|owner| owner.runtime() == runtime && *owner.target() == target)
            {
                self.composer_key = None;
            }
        }
        // Terminal operations still own their bytes. The shared named-root owner
        // retires only independent roots after its durable save barrier.
        self.start_background_save(cx);
        self.retire_released_native_roots(released, window, cx);
        cx.notify();
        self.resume_native_root_close(window, cx);
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
            || (matches!(&action, CheckAction::RetryIfMissing(_))
                && self.payload_local_send_blocks_session(&key.runtime, &key.session))
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
