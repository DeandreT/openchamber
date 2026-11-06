//! Connected prompt send, steer and named-session abort orchestration.
//!
//! Tasks belong to the window, survive selection/runtime switches and always
//! complete against their captured owner. No mutation is retried. A session
//! runs one network chain at a time (a send, a retry or an abort), so a steer
//! waits for an earlier send's requests to finish before its own leave.

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use gpui_kit::{Context, Window};
use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::ids::{MessageId, PartId, SessionId};
use openchamber_native_core::pending_requests::RequestKey;
use openchamber_native_core::transcript::{Message, MessageInfo, Part, Role, TranscriptEvent};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::prompt::{
    AbortError, Delivery, Prompt, PromptAdmission, PromptReceipt, SendError, SendStage,
};
use openchamber_native_transport::prompt_parts::PromptParts;

use crate::connection::ConnectionState;
use crate::locale;
use crate::network::NetworkTask;
use crate::send_attempt::{self, Admission, SendAttempt, SendOrigin, SessionKey};
use crate::server_sync::apply_list_change;
use crate::workspace::Workspace;

/// How long a first Escape keeps a stop armed.
pub(crate) const ABORT_PRIME: std::time::Duration = std::time::Duration::from_secs(3);

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
    pub prompt: Prompt,
    pub requests: Vec<RequestKey>,
    /// Context steps an observation found admitted; a retry skips them.
    pub admitted: Vec<MessageId>,
    /// Whether the composer's text is what this send carries, so preparing
    /// it clears the composer. A queued message sent from its row is not.
    pub consumes_composer: bool,
    /// Whether this send supersedes open requests: it rejects them first and
    /// refuses to post while any is open. A queued message sent from its row
    /// does neither, as the web's queued-only send skips dismissal.
    pub supersedes: bool,
}

/// What a new send carries: the prompt's text and parts, the selection it
/// is sent with, and the draft a definite failure gives back.
pub(crate) struct Outgoing {
    pub draft: Draft,
    pub text: String,
    pub parts: PromptParts,
    pub model: Option<openchamber_native_core::send_config::ModelChoice>,
    pub agent: Option<String>,
    pub delivery: Option<Delivery>,
    pub consumes_composer: bool,
    pub supersedes: bool,
    /// A server-owned prefix keeps this identity across lost reservation replies.
    pub reserved_id: Option<(MessageId, crate::send_attempt::QueueReservation)>,
    /// The user's composer effort, remembered once this send succeeds.
    pub user_effort: Option<crate::effort_preferences::UserEffort>,
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
            && !self.close_in_progress()
            && self
                .send_selection()
                .is_some_and(|(_, selection)| selection.model.is_some())
            && self.selected.as_ref().is_some_and(|session| {
                self.prompt_allowed(&self.mutation_key(session))
                    && !self.queue_preview_pending(session)
                    && !self.payload_take_blocks_session(&self.runtime, session)
                    && self.send_attempts.can_send(&self.mutation_key(session))
                    && self.creations.get(&self.mutation_key(session)).is_none()
            })
            && self.connection.as_ref().is_some_and(|connection| {
                matches!(connection.state, ConnectionState::Connected)
                    && connection.client.is_some()
            })
    }

    /// Sends the composer now. While the session works, only a send that
    /// supersedes open requests goes through; queueing is a separate path.
    pub(crate) fn submit_connected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.submit_connected_with(None, window, cx);
    }

    /// `Some(Steer)` hands the composer to the running turn at its next
    /// steering boundary; like the web composer, it applies only while the
    /// session works, and a session with an uncertain attempt still waits.
    pub(crate) fn submit_connected_with(
        &mut self,
        delivery: Option<Delivery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_composer(delivery, false, window, cx);
    }

    /// Sends the composer now even while the session works, leaving the
    /// delivery to OpenCode, which steers it in (Ctrl+Enter).
    pub(crate) fn submit_connected_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.submit_composer(None, true, window, cx);
    }

    fn submit_composer(
        &mut self,
        delivery: Option<Delivery>,
        now: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.persistence_ready() || self.close_in_progress() {
            return;
        }
        let Some(session) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&session);
        let busy = self.is_streaming(&session);
        let admitted_while_busy = match delivery {
            Some(Delivery::Steer) => busy,
            _ => now || !busy || !self.requests_for_send(&key).is_empty(),
        };
        if !self.prompt_allowed(&key)
            || self.queue_preview_pending(&session)
            || self.payload_take_blocks_session(&key.runtime, &session)
            || !self.send_attempts.can_send(&key)
            || self.creations.get(&key).is_some()
            || !admitted_while_busy
        {
            return;
        }
        if !self.open_sessions.contains_key(&session) || !self.composer_has_sendable_content(cx) {
            return;
        }
        if let Some(plain) = self.plain_composer_text(&key, cx) {
            self.send_plain_composer(key, plain, delivery, window, cx);
            return;
        }
        // This actor owns the complete rich capture and every failure. Falling
        // back to Outgoing would send only the native draft's narrower content.
        self.begin_local_send(delivery, window, cx);
    }

    /// The composer's draft when preparation would send it unchanged: text
    /// only, with no files, context, references, mentions, skills or snippets.
    /// Such a send keeps its optimistic message and attempt recovery.
    fn plain_composer_text(&self, key: &SessionKey, cx: &Context<Self>) -> Option<Draft> {
        let owner = self.composer_key.as_ref().filter(|owner| {
            owner.runtime() == &key.runtime
                && owner.target() == &DraftTarget::Session(key.session.clone())
        })?;
        if self.composer_documents.contains(owner)
            || self.attachment_storage.has_pending(owner)
            // Agents, skills and metadata travel only with the rich capture.
            || self.drafts.get(owner).is_some_and(|draft| draft.content != Default::default())
        {
            return None;
        }
        let composer = self.composer.read(cx);
        let draft = Draft {
            text: composer.value().to_string(),
            selection: composer.selected_range(),
            ..Draft::default()
        };
        (!draft.text.trim().is_empty()
            && !crate::composer_preparation::text_needs_preparation(&draft.text))
        .then_some(draft)
    }

    fn send_plain_composer(
        &mut self,
        key: SessionKey,
        draft: Draft,
        delivery: Option<Delivery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(owner) = self.composer_key.clone() else {
            return;
        };
        let Some((_, selection)) = self.send_selection() else {
            return;
        };
        let Some(model) = selection.model.clone() else {
            return;
        };
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
        let user_effort = Some(self.capture_user_effort(&model));
        self.send_outgoing(
            key,
            owner,
            Outgoing {
                text: draft.text.clone(),
                draft,
                parts: PromptParts::default(),
                model: selection.model,
                agent: selection.agent_name,
                delivery,
                consumes_composer: true,
                supersedes: true,
                reserved_id: None,
                user_effort,
            },
            (client, executor),
            window,
            cx,
        );
    }

    /// Starts one send of `outgoing` to the session of `key` as a new
    /// attempt: its optimistic message, its durable preparation and its
    /// chain. False when the session cannot take a new identity now.
    pub(crate) fn send_outgoing(
        &mut self,
        key: SessionKey,
        owner: DraftKey,
        outgoing: Outgoing,
        (client, executor): (ServerClient, tokio::runtime::Handle),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.close_in_progress()
            || self.payload_take_blocks_session(&key.runtime, &key.session)
            || self.payload_prefix_blocks_session(&key.runtime, &key.session)
            || self.payload_local_send_blocks_session(&key.runtime, &key.session)
        {
            return false;
        }
        let Outgoing {
            draft,
            text,
            parts,
            model,
            agent,
            delivery,
            consumes_composer,
            supersedes,
            reserved_id,
            user_effort,
        } = outgoing;
        let (fresh_id, created) = self.message_ids.next();
        let queue_owned = reserved_id.is_some();
        let (message_id, reservation) = match reserved_id {
            Some((id, reservation)) => (id, Some(reservation)),
            None => (fresh_id, None),
        };
        let prompt = Prompt {
            message_id: message_id.clone(),
            text: text.clone(),
            parts,
            model,
            agent,
            delivery,
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
                synthetic: None,
            },
            parts: vec![Part::text(
                PartId::new(format!("{message_id}-local-text")),
                text,
            )],
        };
        let attempt = SendAttempt {
            origin: SendOrigin::Existing,
            owner,
            resources: crate::attachment_preparation::prepared_resources(&draft),
            draft,
            prompt: prompt.clone(),
            optimistic,
            admission: Admission::Preparing,
            supersedes,
            queue_owned,
            reservation,
        };
        if !self.send_attempts.begin(key.clone(), attempt) {
            return false;
        }
        if let Some(effort) = user_effort {
            self.effort_preferences
                .retain_user_send(&key.runtime, &message_id, effort);
        }
        if queue_owned {
            self.save_composer_draft(cx);
            if self.validate_queue_intent_admission().is_err() {
                // Preview has no side effects. This identity has never reserved
                // or sent, so failure must not leave an unretryable large intent.
                if let Some(attempt) = self.send_attempts.release_unreserved(&key, &message_id) {
                    self.return_to_draft(&key, attempt, window, cx);
                }
                self.notify_mutation_failure("chat.queuedMessage.toast.takeFailed", window, cx);
                cx.notify();
                return false;
            }
        }
        let prompt_allowed = self.prompt_allowed(&key);
        let captured = CapturedPrompt {
            prompt,
            requests: if supersedes {
                self.requests_for_send(&key)
            } else {
                Vec::new()
            },
            admitted: Vec::new(),
            consumes_composer,
            supersedes,
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
        true
    }

    /// Resends the steps of an uncertain attempt that an observation did not
    /// find: the context after `admitted`, then the prompt, under their IDs.
    pub(crate) fn retry_persisted_prompt(
        &mut self,
        key: &SessionKey,
        id: &MessageId,
        requests: Vec<RequestKey>,
        admitted: Vec<MessageId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.persistence_ready()
            || !self.prompt_allowed(key)
            || self.payload_local_send_blocks_session(&key.runtime, &key.session)
        {
            return;
        }
        let Some(attempt) = self.send_attempts.attempt(key, id) else {
            return;
        };
        let prompt = attempt.prompt.clone();
        if attempt.origin == SendOrigin::Created {
            self.retry_created_prompt(key, requests, admitted, window, cx);
            return;
        }
        // A send that left open requests alone retries the same way.
        let supersedes = attempt.supersedes;
        let requests = if supersedes { requests } else { Vec::new() };
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
        if !self.send_attempts.begin_retry(key, id) {
            return;
        }
        self.start_durable_send(
            key.clone(),
            CapturedPrompt {
                prompt,
                requests,
                admitted,
                consumes_composer: false,
                supersedes,
            },
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
        if self.persistence_ready()
            && !self.payload_local_send_blocks_session(&key.runtime, &key.session)
            && self
                .send_attempts
                .begin_retry(&key, &prompt.prompt.message_id)
        {
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
            admitted,
            consumes_composer,
            supersedes,
        } = captured;
        let id = prompt.message_id.clone();
        if matches!(kind, DispatchKind::Retry) {
            self.effort_preferences.forget_user_send(&key.runtime, &id);
        }
        let reservation = self
            .send_attempts
            .attempt(&key, &id)
            .and_then(|attempt| attempt.reservation.clone());
        let directory = self
            .send_attempts
            .attempt(&key, &id)
            .map(|attempt| attempt.owner.directory().to_owned());
        let Some((revision, saved)) = self.persistence_barrier(cx) else {
            self.effort_preferences.forget_user_send(&key.runtime, &id);
            self.send_attempts.finish(
                &key,
                &id,
                Err(match kind {
                    DispatchKind::New => PromptAdmission::NotAttempted,
                    DispatchKind::Retry | DispatchKind::Created => PromptAdmission::Unknown,
                }),
            );
            cx.notify();
            return;
        };
        let task_key = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let saved = saved
                .await
                .unwrap_or(Err(crate::journal_worker::JournalFailure::WorkerStopped));
            let preparation = this.update_in(cx, |this, _, cx| {
                if !this.record_persistence(revision, &saved, cx) || !this.persistence_ready()
                    || this.payload_take_blocks_session(&key.runtime, &key.session)
                     || this.payload_prefix_blocks_session(&key.runtime, &key.session)
                     || this.payload_local_send_blocks_session(&key.runtime, &key.session)
                    || !allowed || (key.runtime == this.runtime && !this.prompt_allowed(&key))
                    || !this.send_attempts.attempt(&key, &id).is_some_and(|attempt| attempt.origin != SendOrigin::Deleted) {
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
                let current = this.send_attempts.attempt(&key, &id).is_some();
                let deleted = this
                    .send_attempts
                    .attempt(&key, &id)
                    .is_some_and(|attempt| attempt.origin == SendOrigin::Deleted);
                // Retain the captured runtime's policy across a switch. While it is
                // still current, metadata updates can revoke permission during the save.
                let allowed = allowed && (key.runtime != this.runtime || this.prompt_allowed(&key));
                // Switching runtimes invalidates the live authority needed to
                // finish a supersession chain, even if its captured cards settled.
                let supersession_current = superseded.is_empty() || key.runtime == this.runtime;
                let refused = !durable || !current || deleted || !allowed || !cancelled || !supersession_current
                    || this.payload_local_send_blocks_session(&key.runtime, &key.session)
                    || (supersedes && !this.requests_for_send(&key).is_empty());
                // Stop pressed during preparation or request rejection wins:
                // nothing of this send leaves beside the interrupt.
                let stopped = !refused && !this.send_attempts.dispatch(&key, &id);
                if refused || stopped {
                    this.effort_preferences.forget_user_send(&key.runtime, &id);
                    // Releases the chain even when the attempt is gone with its
                    // deleted session.
                    let unreserved = matches!(kind, DispatchKind::New) && reservation.is_some();
                    let rejected = if unreserved {
                        // No reservation request has left this first chain.
                        this.send_attempts.release_unreserved(&key, &id)
                    } else { this.send_attempts.finish(
                        &key,
                        &id,
                        Err(match kind {
                            DispatchKind::New => PromptAdmission::NotAttempted,
                            DispatchKind::Retry | DispatchKind::Created => {
                                PromptAdmission::Unknown
                            }
                        }),
                    ) };
                    if let Some(attempt) = rejected {
                        this.return_to_draft(&key, attempt, window, cx);
                        if stopped {
                            this.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
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
                        this.effort_preferences.forget_user_send(&key.runtime, &id);
                        this.mutation_tasks.remove(&key);
                        return false;
                    }
                    let attempt = this
                        .send_attempts
                        .attempt(&key, &id)
                        .expect("prepared attempt remains");
                    let owner = attempt.owner.clone();
                    let draft = attempt.draft.clone();
                    let optimistic = attempt.optimistic.clone();
                    if consumes_composer {
                        this.consume_draft_capture(&owner, &draft, window, cx);
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
                if let Some(intent) = reservation {
                    let result = crate::legacy_queue_reservation::reserve_captured_prefix(
                        &client, &session, directory.as_deref().unwrap_or_default(), &prompt, &intent,
                    ).await;
                    if let Err(source) = result {
                        return Err(SendError {
                            session_id: session,
                            message_id: prompt.message_id.clone(),
                            stage: SendStage::Reservation,
                            admission: PromptAdmission::Unknown,
                            source,
                        });
                    }
                }
                client.resume_prompt(&session, &prompt, &admitted).await
            });
            let result = request.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_send(&key, &id, result, window, cx);
                this.mutation_tasks.remove(&key);
            });
        });
        self.mutation_tasks.insert(task_key, task);
        cx.notify();
    }

    pub(crate) fn finish_send(
        &mut self,
        key: &SessionKey,
        id: &MessageId,
        result: Option<Result<PromptReceipt, SendError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let effort = self.effort_preferences.take_user_send(&key.runtime, id);
        if matches!(&result, Some(Err(error))
            if error.stage == SendStage::Reservation
                && matches!(error.source, openchamber_native_transport::error::TransportError::QueuePrefixUnavailable))
            && let Some(attempt) = self.send_attempts.release_unreserved(key, id)
        {
            let space =
                openchamber_native_core::message_queue::queue_owner(attempt.owner.directory())
                    .map(str::to_owned);
            self.recovery.clear_finished(key, id);
            self.return_to_draft(key, attempt, window, cx);
            self.queue_persistence(cx);
            self.notify_mutation_failure("chat.queuedMessage.toast.takeFailed", window, cx);
            self.read_queue_owner(space, cx);
            cx.notify();
            return;
        }
        // A lost task result cannot establish that the server rejected a POST.
        let admission = match &result {
            Some(Ok(_)) => Ok(()),
            Some(Err(error)) => Err(error.admission),
            None => Err(PromptAdmission::Unknown),
        };
        if self.send_attempts.attempt(key, id).is_none() {
            // Its session was deleted meanwhile: only release the chain.
            self.send_attempts.finish(key, id, admission);
            cx.notify();
            return;
        }
        // A retry ends uncertain whatever its outcome; a definite refusal of
        // it is what makes the attempt one the user may move to the composer.
        let retried = self
            .send_attempts
            .attempt(key, id)
            .is_some_and(|attempt| attempt.admission == Admission::Uncertain);
        if !retried
            && matches!(&result, Some(Ok(_)))
            && self
                .send_attempts
                .attempt(key, id)
                .is_some_and(|attempt| attempt.origin != SendOrigin::Deleted)
            && let Some(effort) = effort
        {
            self.record_user_effort(effort, cx);
        }
        // Only a definite refusal, or local validation, will repeat; a lost
        // selection or admission answer takes the unknown path below.
        let refused = matches!(
            &result,
            Some(Err(error)) if error.stage == SendStage::Validation || error.is_definite()
        );
        if admission.is_ok()
            && key.runtime == self.runtime
            && self
                .send_attempts
                .attempt(key, id)
                .is_some_and(|attempt| attempt.queue_owned)
        {
            let directory = self
                .send_attempts
                .attempt(key, id)
                .expect("retained attempt")
                .owner
                .directory();
            self.read_queue_owner(
                openchamber_native_core::message_queue::queue_owner(directory).map(str::to_owned),
                cx,
            );
        }
        let failed = self.send_attempts.finish(key, id, admission);
        if let Some(attempt) = failed {
            self.return_to_draft(key, attempt, window, cx);
            self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
        } else if retried
            && refused
            && self
                .send_attempts
                .attempt(key, id)
                .is_some_and(|attempt| !attempt.queue_owned)
        {
            // The server refused the same request again; another check
            // cannot change that, so the row offers the composer instead.
            self.mark_refused(key, id);
            self.notify_mutation_failure("chat.nativeRecovery.refused", window, cx);
        } else if self.runtime == key.runtime {
            if admission.is_err() && self.send_attempts.attempt(key, id).is_some() {
                self.notify_mutation_failure("chat.nativeRecovery.unknown", window, cx);
            }
            // The existing read path merges exact IDs. An absent ID leaves the
            // attempt locked and preserved; history never infers rejection.
            self.load_history(&key.session, cx);
            if admission.is_err() {
                self.check_recovery(key, id, window, cx);
            }
        }
        self.queue_persistence(cx);
        cx.notify();
    }

    /// Returns a removed attempt's prompt to its original draft, after any
    /// newer text, and drops its optimistic message.
    pub(crate) fn return_to_draft(
        &mut self,
        key: &SessionKey,
        attempt: SendAttempt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_composer_draft(cx);
        let Ok(draft) =
            send_attempt::restore_failed(self.drafts.get(&attempt.owner), &attempt.draft)
        else {
            let id = attempt.prompt.message_id.clone();
            // The caller removed this exact capture in the same foreground
            // transition. No other unresolved identity can have started here.
            assert!(
                self.send_attempts
                    .retain_failed(key.clone(), attempt)
                    .is_ok(),
                "a removed attempt retains its original recovery slot"
            );
            self.mark_refused(key, &id);
            self.notify_mutation_failure("chat.nativeRecovery.contentConflict", window, cx);
            self.queue_persistence(cx);
            return;
        };
        if self.runtime == *attempt.owner.runtime()
            && let DraftTarget::Session(session) = attempt.owner.target()
            && let Some(open) = self.open_sessions.get_mut(session)
        {
            let change = open.transcript.apply(TranscriptEvent::MessageRemoved {
                message_id: attempt.prompt.message_id.clone(),
            });
            apply_list_change(open, &change, cx);
        }
        self.drafts.save(attempt.owner.clone(), draft);
        if self.composer_key.as_ref() == Some(&attempt.owner) {
            self.restore_composer_draft(window, cx);
        }
    }

    /// Escape in the composer of a working session (`useKeyboardShortcuts`'
    /// abort priming): the first press arms a stop for that session for
    /// three seconds, a second press within them stops it. Escape elsewhere,
    /// in an idle session or for another session disarms it.
    pub(crate) fn escape_to_stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::Focusable;
        let now = std::time::Instant::now();
        let primed = self.abort_primed.take();
        let from_composer = self.composer.read(cx).focus_handle(cx).is_focused(window);
        let Some(session) = self
            .selected
            .clone()
            .filter(|session| from_composer && self.is_streaming(session))
        else {
            return;
        };
        match primed {
            Some((armed, until)) if armed == session && now < until => self.stop(window, cx),
            _ => self.abort_primed = Some((session, now + ABORT_PRIME)),
        }
    }

    pub(crate) fn abort_connected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&session);
        let cancelled_local = self
            .queue
            .local_sends
            .stop_before_dispatch(&key.runtime, &session);
        if self
            .queue
            .local_sends
            .dispatch_in_flight(&key.runtime, &session)
            || (!cancelled_local && self.payload_local_send_blocks_session(&key.runtime, &session))
            || self.payload_take_blocks_session(&key.runtime, &session)
            || self.payload_prefix_blocks_session(&key.runtime, &session)
        {
            self.notify_mutation_failure("chat.nativeRecovery.stopWaits", window, cx);
            return;
        }
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        if !self.send_attempts.begin_abort(key.clone()) {
            if self.send_attempts.abort_waits(&key) {
                self.notify_mutation_failure("chat.nativeRecovery.stopWaits", window, cx);
            }
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
                this.abort_tasks.remove(&key);
            });
        });
        self.abort_tasks.insert(task_key, task);
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

    pub(crate) fn notify_mutation_failure(
        &self,
        key: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.push_notification(Notification::error(locale::strings(cx).get(key)), cx);
    }
}
