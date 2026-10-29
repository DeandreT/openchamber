//! Connected text send and named-session abort orchestration.
//!
//! Tasks belong to the window, survive selection/runtime switches and always
//! complete against their captured owner. No mutation is retried.

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use gpui_kit::{Context, Window};
use openchamber_native_core::draft::{Draft, DraftTarget};
use openchamber_native_core::ids::{PartId, SessionId};
use openchamber_native_core::pending_requests::RequestKey;
use openchamber_native_core::transcript::{Message, MessageInfo, Part, Role, TranscriptEvent};
use openchamber_native_transport::prompt::{
    AbortError, PromptAdmission, PromptReceipt, SendError, TextPrompt,
};

use crate::connection::ConnectionState;
use crate::locale;
use crate::network::NetworkTask;
use crate::send_attempt::{self, Admission, SendAttempt, SendOrigin, SessionKey};
use crate::server_sync::apply_list_change;
use crate::workspace::Workspace;

#[derive(Clone, Copy)]
enum DispatchKind {
    New,
    Retry,
    Created,
}

struct SendDispatch {
    prompt_allowed: bool,
    client: openchamber_native_transport::client::ServerClient,
    executor: tokio::runtime::Handle,
    kind: DispatchKind,
}

/// One explicit activation's prompt and cancellation scope. Never journaled.
pub(crate) struct CapturedPrompt {
    pub prompt: TextPrompt,
    pub requests: Vec<RequestKey>,
}

impl Workspace {
    /// Native has no browser-local allowPromptingSubagentSessions preference.
    /// Unknown records cannot authorize a dispatch to an existing session.
    pub(crate) fn prompt_allowed(&self, key: &SessionKey) -> bool {
        key.runtime == self.runtime
            && self
                .sessions
                .get(&key.session)
                .is_some_and(|session| !session.prompt_read_only(false, false))
    }

    pub(crate) fn mutation_key(&self, session: &SessionId) -> SessionKey {
        SessionKey {
            runtime: self.runtime.clone(),
            session: session.clone(),
        }
    }

    pub(crate) fn can_send_connected(&self) -> bool {
        self.persistence_ready()
            && self
                .send_selection()
                .is_some_and(|(_, selection)| selection.model.is_some())
            && self.selected.as_ref().is_some_and(|session| {
                self.prompt_allowed(&self.mutation_key(session))
                    && self.send_attempts.can_send(&self.mutation_key(session))
                    && self.creations.get(&self.mutation_key(session)).is_none()
            })
            && self.connection.as_ref().is_some_and(|connection| {
                matches!(connection.state, ConnectionState::Connected)
                    && connection.client.is_some()
            })
    }

    pub(crate) fn submit_connected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.persistence_ready() {
            return;
        }
        let Some(session) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&session);
        if !self.prompt_allowed(&key)
            || !self.send_attempts.can_send(&key)
            || self.creations.get(&key).is_some()
            || (self.is_streaming(&session) && self.requests_for_send(&key).is_empty())
        {
            return;
        }
        let Some(owner) = self.composer_key.clone().filter(|owner| {
            owner.runtime() == &key.runtime
                && owner.target() == &DraftTarget::Session(session.clone())
        }) else {
            return;
        };
        let Some((_, selection)) = self.send_selection() else {
            return;
        };
        if selection.model.is_none() {
            return;
        }
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
        let executor = connection.network.handle();
        if !self.open_sessions.contains_key(&session) {
            return;
        }
        let composer = self.composer.read(cx);
        let draft = Draft {
            text: composer.value().to_string(),
            selection: composer.selected_range(),
        };
        if draft.text.trim().is_empty() {
            return;
        }
        let (message_id, created) = self.message_ids.next();
        let prompt = TextPrompt {
            message_id: message_id.clone(),
            text: draft.text.clone(),
            model: selection.model,
            agent: selection.agent_name,
            delivery: None,
        };
        let optimistic = Message {
            info: MessageInfo {
                id: message_id.clone(),
                role: Role::User,
                created,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
            },
            parts: vec![Part::text(
                PartId::new(format!("{message_id}-local-text")),
                draft.text.clone(),
            )],
        };
        let attempt = SendAttempt {
            origin: SendOrigin::Existing,
            owner: owner.clone(),
            draft,
            prompt: prompt.clone(),
            optimistic,
            admission: Admission::Preparing,
        };
        if !self.send_attempts.begin(key.clone(), attempt) {
            return;
        }
        let prompt_allowed = self.prompt_allowed(&key);
        let captured = CapturedPrompt {
            prompt,
            requests: self.requests_for_send(&key),
        };
        self.start_durable_send(
            key,
            captured,
            SendDispatch {
                prompt_allowed,
                client,
                executor,
                kind: DispatchKind::New,
            },
            window,
            cx,
        );
    }

    pub(crate) fn retry_persisted_prompt(
        &mut self,
        key: &SessionKey,
        id: &openchamber_native_core::ids::MessageId,
        requests: Vec<RequestKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.persistence_ready() || !self.prompt_allowed(key) {
            return;
        }
        let Some(attempt) = self
            .send_attempts
            .attempt(key)
            .filter(|attempt| attempt.prompt.message_id == *id)
        else {
            return;
        };
        let prompt = attempt.prompt.clone();
        if attempt.origin == SendOrigin::Created {
            self.retry_created_prompt(key, requests, window, cx);
            return;
        }
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
        let executor = connection.network.handle();
        if !self.send_attempts.begin_retry(key) {
            return;
        }
        self.start_durable_send(
            key.clone(),
            CapturedPrompt { prompt, requests },
            SendDispatch {
                prompt_allowed: self.prompt_allowed(key),
                client,
                executor,
                kind: DispatchKind::Retry,
            },
            window,
            cx,
        );
    }

    pub(crate) fn start_created_send(
        &mut self,
        key: SessionKey,
        prompt: CapturedPrompt,
        session: &openchamber_native_core::session::SessionRecord,
        client: openchamber_native_transport::client::ServerClient,
        executor: tokio::runtime::Handle,
        context: (&mut Window, &mut Context<Self>),
    ) {
        let (window, cx) = context;
        self.start_durable_send(
            key,
            prompt,
            SendDispatch {
                prompt_allowed: !session.prompt_read_only(false, false),
                client,
                executor,
                kind: DispatchKind::Created,
            },
            window,
            cx,
        );
    }

    pub(crate) fn dispatch_created_retry(
        &mut self,
        key: SessionKey,
        prompt: CapturedPrompt,
        session: &openchamber_native_core::session::SessionRecord,
        client: openchamber_native_transport::client::ServerClient,
        executor: tokio::runtime::Handle,
        context: (&mut Window, &mut Context<Self>),
    ) {
        let (window, cx) = context;
        if self.persistence_ready() && self.send_attempts.begin_retry(&key) {
            self.start_durable_send(
                key,
                prompt,
                SendDispatch {
                    prompt_allowed: !session.prompt_read_only(false, false),
                    client,
                    executor,
                    kind: DispatchKind::Retry,
                },
                window,
                cx,
            );
        }
    }

    fn start_durable_send(
        &mut self,
        key: SessionKey,
        captured: CapturedPrompt,
        dispatch: SendDispatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SendDispatch {
            prompt_allowed: allowed,
            client,
            executor,
            kind,
        } = dispatch;
        let CapturedPrompt {
            prompt,
            requests: superseded,
        } = captured;
        let Some((revision, saved)) = self.persistence_barrier(cx) else {
            self.send_attempts.finish(
                &key,
                Err(match kind {
                    DispatchKind::New => PromptAdmission::NotAttempted,
                    DispatchKind::Retry | DispatchKind::Created => PromptAdmission::Unknown,
                }),
            );
            cx.notify();
            return;
        };
        let id = prompt.message_id.clone();
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let saved = saved
                .await
                .unwrap_or(Err(crate::journal_worker::JournalFailure::WorkerStopped));
            let preparation = this.update_in(cx, |this, _, cx| {
                if !this.record_persistence(revision, &saved, cx) || !this.persistence_ready()
                    || !allowed || (key.runtime == this.runtime && !this.prompt_allowed(&key))
                    || !this.send_attempts.attempt(&key).is_some_and(|attempt| attempt.prompt.message_id == id && attempt.origin != SendOrigin::Deleted) {
                    return Err(());
                }
                this.prepare_supersession(&superseded)
            });
            let mut cancelled = true;
            if let Ok(Ok(attempts)) = preparation {
                for attempt in attempts {
                    let request_key = attempt.key().clone();
                    let captured = request_key.clone();
                    let client = client.clone();
                    let request = NetworkTask::spawn_on(&executor, async move {
                        let reply = match captured.id {
                            openchamber_native_core::pending_requests::RequestId::Permission(_) => crate::blocking_requests::RequestReply::Permission(openchamber_native_core::permission::PermissionReply::Reject),
                            openchamber_native_core::pending_requests::RequestId::Form(_) => crate::blocking_requests::RequestReply::Dismiss,
                        };
                        crate::blocking_requests::respond(&client, &captured, &reply).await
                    });
                    let result = request.await;
                    let success = matches!(result, Some(Ok(())) | Some(Err(openchamber_native_transport::requests::RequestError::PermissionNotFound | openchamber_native_transport::requests::RequestError::FormNotFound | openchamber_native_transport::requests::RequestError::FormAlreadySettled)));
                    cancelled &= success;
                    let _ = this.update_in(cx, |this, window, cx| {
                        if let Some(store) = this.blocking.stores.get_mut(&request_key.scope) {
                            if success { let _ = store.settle(&request_key); }
                            else { let _ = store.reply_failed(attempt, openchamber_native_core::pending_requests::ReplyFailure::Unknown); this.blocking.errors.insert(request_key); }
                        }
                        this.sync_request_drafts(window, cx); cx.notify();
                    });
                }
            } else { cancelled = false; }
            let dispatch = this.update_in(cx, |this, window, cx| {
                let durable =
                    this.record_persistence(revision, &saved, cx) && this.persistence_ready();
                let current = this
                    .send_attempts
                    .attempt(&key)
                    .is_some_and(|attempt| attempt.prompt.message_id == id);
                let deleted = this
                    .send_attempts
                    .attempt(&key)
                    .is_some_and(|attempt| attempt.origin == SendOrigin::Deleted);
                // Retain the captured runtime's policy across a switch. While it is
                // still current, metadata updates can revoke permission during the save.
                let allowed = allowed && (key.runtime != this.runtime || this.prompt_allowed(&key));
                // Switching runtimes invalidates the live authority needed to
                // finish a supersession chain, even if its captured cards settled.
                let supersession_current = superseded.is_empty() || key.runtime == this.runtime;
                if !durable || !current || deleted || !allowed || !cancelled || !supersession_current || !this.requests_for_send(&key).is_empty() {
                    if current {
                        let rejected = this.send_attempts.finish(
                            &key,
                            Err(match kind {
                                DispatchKind::New => PromptAdmission::NotAttempted,
                                DispatchKind::Retry | DispatchKind::Created => {
                                    PromptAdmission::Unknown
                                }
                            }),
                        );
                        if let Some(attempt) = rejected {
                            this.save_composer_draft(cx);
                            let draft = send_attempt::restore_failed(this.drafts.get(&attempt.owner), attempt.draft);
                            this.drafts.save(attempt.owner.clone(), draft);
                            if this.composer_key.as_ref() == Some(&attempt.owner) { this.restore_composer_draft(window, cx); }
                        }
                    }
                    this.mutation_tasks.remove(&key);
                    if durable && current {
                        // The prepared journal excluded this text from drafts. The
                        // refusal leaves the editor intact, so persist that owner again.
                        this.queue_persistence(cx);
                    }
                    cx.notify();
                    return false;
                }
                if matches!(kind, DispatchKind::New | DispatchKind::Created) {
                    if !this.send_attempts.prepared(&key, &id) {
                        this.mutation_tasks.remove(&key);
                        return false;
                    }
                    let attempt = this
                        .send_attempts
                        .attempt(&key)
                        .expect("prepared attempt remains");
                    let owner = attempt.owner.clone();
                    let text = attempt.draft.text.clone();
                    let optimistic = attempt.optimistic.clone();
                    if matches!(kind, DispatchKind::New)
                        && this
                            .drafts
                            .get(&owner)
                            .is_some_and(|draft| draft.text == text)
                    {
                        this.drafts.take(&owner);
                    }
                    if matches!(kind, DispatchKind::New)
                        && this.composer_key.as_ref() == Some(&owner)
                        && this.composer.read(cx).value().as_ref() == text
                    {
                        this.composer
                            .update(cx, |composer, cx| composer.set_value("", window, cx));
                    }
                    if key.runtime == this.runtime
                        && let Some(open) = this.open_sessions.get_mut(&key.session)
                    {
                        let change = open.transcript.push(optimistic);
                        apply_list_change(open, &change, cx);
                        open.list.scroll_to_end();
                    }
                    this.queue_persistence(cx);
                }
                cx.notify();
                true
            });
            if !matches!(dispatch, Ok(true)) {
                return;
            }
            let session = key.session.clone();
            let request = NetworkTask::spawn_on(&executor, async move {
                client.send_text(&session, &prompt).await
            });
            let result = request.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this
                    .send_attempts
                    .attempt(&key)
                    .is_some_and(|attempt| attempt.prompt.message_id == id)
                {
                    this.finish_send(&key, result, window, cx);
                }
                this.mutation_tasks.remove(&key);
            });
        });
        self.mutation_tasks.insert(task_key, task);
        cx.notify();
    }

    pub(crate) fn finish_send(
        &mut self,
        key: &SessionKey,
        result: Option<Result<PromptReceipt, SendError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A lost task result cannot establish that the server rejected a POST.
        let admission = match result {
            Some(Ok(_)) => Ok(()),
            Some(Err(error)) => Err(error.admission),
            None => Err(PromptAdmission::Unknown),
        };
        let failed = self.send_attempts.finish(key, admission);
        if let Some(attempt) = failed {
            if self.runtime == key.runtime
                && let Some(open) = self.open_sessions.get_mut(&key.session)
            {
                let change = open.transcript.apply(TranscriptEvent::MessageRemoved {
                    message_id: attempt.prompt.message_id,
                });
                apply_list_change(open, &change, cx);
            }
            self.save_composer_draft(cx);
            let draft =
                send_attempt::restore_failed(self.drafts.get(&attempt.owner), attempt.draft);
            self.drafts.save(attempt.owner.clone(), draft);
            if self.composer_key.as_ref() == Some(&attempt.owner) {
                self.restore_composer_draft(window, cx);
            }
            self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
        } else if self.runtime == key.runtime {
            if admission.is_err() && self.send_attempts.attempt(key).is_some() {
                self.notify_mutation_failure("chat.nativeRecovery.unknown", window, cx);
            }
            // The existing read path merges exact IDs. An absent ID leaves the
            // attempt locked and preserved; history never infers rejection.
            self.load_history(&key.session, cx);
            if admission.is_err() {
                self.check_recovery(key, window, cx);
            }
        }
        self.queue_persistence(cx);
        cx.notify();
    }

    pub(crate) fn abort_connected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&session);
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        if !self.send_attempts.begin_abort(key.clone()) {
            return;
        }
        let request = connection
            .network
            .run(async move { client.abort_session(&session).await });
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_abort(&key, result, window, cx);
                this.mutation_tasks.remove(&key);
            });
        });
        self.mutation_tasks.insert(task_key, task);
        cx.notify();
    }

    pub(crate) fn finish_abort(
        &mut self,
        key: &SessionKey,
        result: Option<Result<bool, AbortError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.send_attempts.finish_abort(key);
        // Both successful admission and abort leave live status to the
        // server's status stream. Failure cannot manufacture idle.
        if !matches!(result, Some(Ok(_))) {
            self.notify_mutation_failure("chat.nativeRecovery.abortFailed", window, cx);
        }
        cx.notify();
    }

    fn notify_mutation_failure(
        &self,
        key: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.push_notification(Notification::error(locale::strings(cx).get(key)), cx);
    }
}
