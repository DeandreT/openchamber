//! Exact-ID observations and explicit retries of retained prompts.

use std::collections::HashMap;

use gpui_kit::{Context, Task, Window};
use openchamber_native_core::ids::MessageId;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::prompt::PromptObservation;

use crate::connection::ConnectionState;
use crate::recovery_view::{RecoveryPhase, RecoveryRow};
use crate::send_attempt::{Admission, SendOrigin, SessionKey};
use crate::server_sync::apply_list_change;
use crate::workspace::Workspace;

#[derive(Clone, Copy)]
enum CheckState {
    Checking,
    NotObserved,
    Failed,
}

#[derive(Clone, Copy)]
enum CheckAction {
    Observe,
    RetryIfMissing,
}

struct Check {
    id: MessageId,
    state: CheckState,
    since: u64,
}

#[derive(Default)]
pub(crate) struct RecoveryState {
    checks: HashMap<SessionKey, Check>,
    tasks: HashMap<SessionKey, Task<()>>,
}

impl RecoveryState {
    pub(crate) fn clear_finished(&mut self, key: &SessionKey) {
        self.checks.remove(key);
        self.tasks.remove(key);
    }
}

impl Workspace {
    pub(crate) fn recovery_items(&self, _cx: &gpui_kit::App) -> Vec<RecoveryRow> {
        let mut rows: Vec<_> = self
            .send_attempts
            .iter()
            .filter(|(key, _)| key.runtime == self.runtime)
            .map(|(key, attempt)| {
                let checking = self
                    .recovery
                    .checks
                    .get(key)
                    .filter(|check| check.id == attempt.prompt.message_id);
                let phase = if attempt.origin == SendOrigin::Deleted {
                    RecoveryPhase::CreateDeleted
                } else if let Some(check) = self.creations.checks.get(key) {
                    match check {
                        crate::create_attempt::CreationCheck::Checking => RecoveryPhase::Checking,
                        crate::create_attempt::CreationCheck::Mismatch => {
                            RecoveryPhase::CreateMismatch
                        }
                        _ => RecoveryPhase::CreateCheckFailed,
                    }
                } else if matches!(
                    checking.map(|check| check.state),
                    Some(CheckState::Checking)
                ) {
                    RecoveryPhase::Checking
                } else if self.send_attempts.is_active(key)
                    && attempt.admission == Admission::Uncertain
                {
                    RecoveryPhase::Sending
                } else {
                    match attempt.admission {
                        Admission::Preparing => RecoveryPhase::Preparing,
                        Admission::Sending { .. } => RecoveryPhase::Sending,
                        Admission::Admitted => RecoveryPhase::Queued,
                        Admission::Uncertain => match checking.map(|check| check.state) {
                            Some(CheckState::Checking) => RecoveryPhase::Checking,
                            Some(CheckState::NotObserved) => RecoveryPhase::NotObserved,
                            Some(CheckState::Failed) => RecoveryPhase::CheckFailed,
                            None => RecoveryPhase::Uncertain,
                        },
                    }
                };
                let connected = self.connection.as_ref().is_some_and(|connection| {
                    matches!(connection.state, ConnectionState::Connected)
                });
                let busy = self.send_attempts.is_active(key)
                    || self.recovery.tasks.contains_key(key)
                    || self.creation_tasks.contains_key(key)
                    || attempt.origin == SendOrigin::Deleted;
                RecoveryRow {
                    key: key.clone(),
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
                        && !busy
                        && self.persistence_ready()
                        && attempt.admission == Admission::Uncertain
                        && !self.creations.checks.contains_key(key)
                        && matches!(
                            checking.map(|check| check.state),
                            Some(CheckState::NotObserved)
                        ),
                }
            })
            .collect();
        rows.extend(self.creation_recovery_items());
        rows.sort_by(|a, b| a.key.session.as_str().cmp(b.key.session.as_str()));
        rows
    }

    pub(crate) fn check_recovery(
        &mut self,
        key: &SessionKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.creations.get(key).is_some() {
            self.check_creation(key, false, window, cx);
            return;
        }
        if self
            .send_attempts
            .attempt(key)
            .is_some_and(|attempt| attempt.origin == SendOrigin::Created)
        {
            self.check_created_send(key, false, window, cx);
            return;
        }
        self.start_recovery_check(key, CheckAction::Observe, window, cx);
    }

    pub(crate) fn retry_recovery(
        &mut self,
        key: &SessionKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.creations.get(key).is_some() {
            self.check_creation(key, true, window, cx);
            return;
        }
        if self
            .send_attempts
            .attempt(key)
            .is_some_and(|attempt| attempt.origin == SendOrigin::Created)
        {
            self.check_created_send(key, true, window, cx);
            return;
        }
        if self.persistence_ready() {
            self.start_recovery_check(key, CheckAction::RetryIfMissing, window, cx);
        }
    }

    pub(crate) fn continue_created_recovery(
        &mut self,
        key: &SessionKey,
        retry: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_recovery_check(
            key,
            if retry {
                CheckAction::RetryIfMissing
            } else {
                CheckAction::Observe
            },
            window,
            cx,
        );
    }

    fn start_recovery_check(
        &mut self,
        key: &SessionKey,
        action: CheckAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.persistence_closing()
            || key.runtime != self.runtime
            || self.send_attempts.is_active(key)
            || self.recovery.tasks.contains_key(key)
            || self.creation_tasks.contains_key(key)
        {
            return;
        }
        let Some(attempt) = self.send_attempts.attempt(key) else {
            return;
        };
        if attempt.admission == Admission::Preparing || attempt.origin == SendOrigin::Deleted {
            return;
        }
        let id = attempt.prompt.message_id.clone();
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
        let message = id.clone();
        let since = self
            .open_sessions
            .get(&key.session)
            .map_or(0, |open| open.transcript.revision());
        let request = connection
            .network
            .run(async move { client.lookup_prompt(&session, &message).await });
        self.recovery.checks.insert(
            key.clone(),
            Check {
                id: id.clone(),
                state: CheckState::Checking,
                since,
            },
        );
        let key = key.clone();
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = request.await.unwrap_or(Err(TransportError::Cancelled));
            let _ = this.update_in(cx, |this, window, cx| {
                this.recovery.tasks.remove(&key);
                this.finish_recovery_check(&key, &id, action, result, window, cx);
            });
        });
        self.recovery.tasks.insert(task_key, task);
        cx.notify();
    }

    fn finish_recovery_check(
        &mut self,
        key: &SessionKey,
        id: &MessageId,
        action: CheckAction,
        result: Result<PromptObservation, TransportError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(check) = self
            .recovery
            .checks
            .get(key)
            .filter(|check| check.id == *id)
        else {
            return;
        };
        let since = check.since;
        if !self
            .send_attempts
            .attempt(key)
            .is_some_and(|attempt| attempt.prompt.message_id == *id)
        {
            self.recovery.checks.remove(key);
            return;
        }
        match result {
            Ok(PromptObservation::Pending(_)) => {
                self.send_attempts.confirm_pending(key, id);
                self.recovery.checks.remove(key);
                self.queue_persistence(cx);
            }
            Ok(PromptObservation::Delivered(message)) => {
                if key.runtime == self.runtime {
                    if let Some(open) = self.open_sessions.get_mut(&key.session) {
                        let change = open.transcript.apply_message_snapshot(*message, since);
                        apply_list_change(open, &change, cx);
                    }
                    self.load_history(&key.session, cx);
                }
                self.send_attempts.observe(key, id);
                self.recovery.checks.remove(key);
                self.queue_persistence(cx);
            }
            Ok(PromptObservation::NotObserved) => {
                self.recovery.checks.insert(
                    key.clone(),
                    Check {
                        id: id.clone(),
                        state: CheckState::NotObserved,
                        since,
                    },
                );
                if matches!(action, CheckAction::RetryIfMissing) && key.runtime == self.runtime {
                    self.retry_persisted_prompt(key, id, window, cx);
                }
            }
            Err(_) => {
                self.recovery.checks.insert(
                    key.clone(),
                    Check {
                        id: id.clone(),
                        state: CheckState::Failed,
                        since,
                    },
                );
            }
        }
        cx.notify();
    }
}
