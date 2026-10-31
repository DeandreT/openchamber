//! The composer's side of the server-owned message queue: the projection
//! the queued strip draws, the reads that fill it, and the queue, edit,
//! send, remove and reorder actions, as `ChatInput.tsx` and
//! `QueuedMessagesStrip.tsx` drive `stores/messageQueueStore.ts`.
//!
//! The server delivers the queue when the session goes idle; nothing here
//! sends a queued message on its own. A queued message the user sends or
//! edits is taken out of the queue first, so it can never go out twice.
//! Sending goes through the ordinary send path (`send_outgoing`) as one
//! durable attempt, which gives the text back to the composer on a definite
//! failure.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use gpui_kit::{Context, Task, Window};
use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::message_queue::{
    FollowUpBehavior, MessageQueue, QueueKey, QueuedItemId, QueuedMessage, QueuedSendConfig,
};
use openchamber_native_core::send_config::ModelChoice;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::message_queue::{
    QueueItemInput, QueueUpdate, ReservedPrefix, TakenMessage,
};
use openchamber_native_transport::prompt::Delivery;
use openchamber_native_transport::prompt_parts::{AgentMention, PromptParts};

use crate::connection::ConnectionState;
use crate::locale;
use crate::mutation_journal::MAX_URI;
use crate::send_attempt;
use crate::session_mutations::Outgoing;
use crate::workspace::Workspace;

/// The queue projection and the work in flight on it. Runtime-scoped: a
/// runtime switch drops the old runtime's projection and every completion
/// started before it.
#[derive(Default)]
pub(crate) struct QueueState {
    pub(crate) projection: MessageQueue,
    /// The list of several queued messages is open; one shared choice, so it
    /// stays across session switches (`messageQueueExpanded`).
    pub(crate) expanded: bool,
    generation: u64,
    /// The connection whose host snapshot was requested.
    read_connection: Option<u64>,
    /// Spaces whose own queue was read on this connection.
    space_reads: HashSet<String>,
    tasks: HashMap<u64, Task<()>>,
    next_task: u64,
    /// Queues a take is changing; a second take waits for it.
    taking: HashSet<QueueKey>,
}

/// What a take is for once the server answered with the message.
enum TakePurpose {
    Edit,
    Send,
}

impl Workspace {
    /// The follow-up behavior the settings choose.
    pub(crate) fn follow_up_behavior(&self) -> FollowUpBehavior {
        self.send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.follow_up_behavior)
            .unwrap_or_default()
    }

    /// The selected session's queue location.
    pub(crate) fn selected_queue_key(&self) -> Option<QueueKey> {
        let session = self.selected_session()?;
        QueueKey::new(
            self.runtime.clone(),
            session.directory.as_deref()?,
            session.id.clone(),
        )
    }

    /// The selected session's queued messages, oldest first.
    pub(crate) fn selected_queue(&self) -> &[QueuedMessage] {
        self.selected_queue_key()
            .map_or(&[], |key| self.queue.projection.queue(&key))
    }

    fn queue_task(&mut self, task: impl FnOnce(u64) -> Task<()>) {
        let id = self.queue.next_task;
        self.queue.next_task += 1;
        let task = task(id);
        self.queue.tasks.insert(id, task);
    }

    fn queue_client(
        &self,
    ) -> Option<(
        openchamber_native_transport::client::ServerClient,
        tokio::runtime::Handle,
    )> {
        let connection = self
            .connection
            .as_ref()
            .filter(|connection| matches!(connection.state, ConnectionState::Connected))?;
        Some((connection.client.clone()?, connection.network.handle()))
    }

    /// Reads the host's whole queue. Runs on every event-stream connection:
    /// queue broadcasts are not replayed, and a read is how a gap heals.
    pub(crate) fn load_message_queue(&mut self, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        self.queue.read_connection = Some(connection.generation);
        self.queue.space_reads.clear();
        self.read_queue_owner(None, cx);
    }

    /// Reads the queue of the selected session's space once per connection,
    /// since that space's own server owns it (`refreshSpaceQueue`).
    pub(crate) fn ensure_space_queue(&mut self, cx: &mut Context<Self>) {
        let Some(space) = self
            .selected_queue_key()
            .and_then(|key| key.owner().map(str::to_owned))
        else {
            return;
        };
        if self.queue.read_connection.is_none() || !self.queue.space_reads.insert(space.clone()) {
            return;
        }
        self.read_queue_owner(Some(space), cx);
    }

    pub(crate) fn read_queue_owner(&mut self, space: Option<String>, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let owner = space.clone();
        let read = connection
            .network
            .run(async move { client.message_queue(owner.as_deref()).await });
        let generation = self.queue.generation;
        let runtime = self.runtime.clone();
        self.queue_task(|id| {
            cx.spawn(async move |this, cx| {
                let result = read.await;
                let _ = this.update(cx, |this, cx| {
                    this.queue.tasks.remove(&id);
                    if generation != this.queue.generation || runtime != this.runtime {
                        return;
                    }
                    match result {
                        Some(Ok(snapshot)) => {
                            this.queue.projection.apply_snapshot(
                                &runtime,
                                space.as_deref(),
                                snapshot.revision,
                                snapshot.sessions,
                            );
                            cx.notify();
                        }
                        // A failed read keeps what the projection held; a
                        // stopped space keeps what was last known of it.
                        Some(Err(error)) => {
                            tracing::warn!(%error, "the message queue could not be read");
                        }
                        None => {}
                    }
                });
            })
        });
    }

    /// A queue the server broadcast after a change.
    pub(crate) fn apply_queue_broadcast(&mut self, update: QueueUpdate, space: Option<String>) {
        let runtime = self.runtime.clone();
        self.queue.projection.apply_session(
            &runtime,
            space.as_deref(),
            update.session,
            update.revision,
        );
    }

    /// A deleted session's queue goes with it; the server drops its copy.
    pub(crate) fn forget_session_queue(&mut self, session: &SessionId) {
        let runtime = self.runtime.clone();
        self.queue.projection.forget_session(&runtime, session);
    }

    /// Leaves `previous`: its projection belongs to its server, and every
    /// completion started for it is dropped.
    pub(crate) fn reset_queue_runtime(
        &mut self,
        previous: &openchamber_native_core::ids::RuntimeId,
    ) {
        self.queue.generation += 1;
        self.queue.projection.reset_runtime(previous);
        self.queue.read_connection = None;
        self.queue.space_reads.clear();
        self.queue.taking.clear();
    }

    /// Applies a mutation's answer, or after a failure rereads the owner's
    /// queue: server state wins over a guess.
    fn settle_queue_mutation(
        &mut self,
        key: &QueueKey,
        result: Option<Result<QueueUpdate, TransportError>>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Some(Ok(update)) => {
                self.queue.projection.apply_session(
                    &key.runtime,
                    key.owner(),
                    update.session,
                    update.revision,
                );
            }
            Some(Err(error)) => {
                tracing::warn!(%error, "a message queue change failed");
                self.read_queue_owner(key.owner().map(str::to_owned), cx);
            }
            None => self.read_queue_owner(key.owner().map(str::to_owned), cx),
        }
        cx.notify();
    }

    /// The draft owner of a session's composer.
    fn session_draft_key(&self, key: &QueueKey) -> Option<DraftKey> {
        DraftKey::new(
            key.runtime.clone(),
            &key.directory,
            DraftTarget::Session(key.session.clone()),
        )
    }

    /// Puts `text` back into `owner`'s draft after anything typed since,
    /// and into the composer when it shows that draft.
    pub(crate) fn give_back_text(
        &mut self,
        owner: &DraftKey,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if text.is_empty() {
            return;
        }
        self.save_composer_draft(cx);
        let failed = Draft {
            selection: text.len()..text.len(),
            text,
        };
        let draft = send_attempt::restore_failed(self.drafts.get(owner), failed);
        self.drafts.save(owner.clone(), draft);
        if self.composer_key.as_ref() == Some(owner) {
            self.restore_composer_draft(window, cx);
        }
        self.queue_persistence(cx);
    }

    fn notify_queue_failure(&self, key: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        window.push_notification(Notification::error(locale::strings(cx).get(key)), cx);
    }

    /// Enter on a follow-up, as the composer's `onKeyDown` routes it.
    /// `/fork`, `/btw` and an open side conversation take the text first, as
    /// the web plans local commands and checks BTW before queueing. Then,
    /// while the session works and the composer has text, Enter queues (even
    /// with requests open, which it leaves alone) or steers in with the queue
    /// by the setting, and Ctrl+Enter sends now; otherwise Enter sends,
    /// taking the queue along.
    pub(crate) fn submit_from_enter(
        &mut self,
        secondary: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected.clone().filter(|_| !self.is_synthetic()) else {
            self.submit(window, cx);
            return;
        };
        if !self.can_start_send(&session) || self.queue_preview_pending(&session) {
            return;
        }
        if self.run_branch_command(window, cx) {
            return;
        }
        if self.btw_takes_composer() {
            self.send_btw_follow_up(window, cx);
            return;
        }
        let has_text = !self.composer.read(cx).value().trim().is_empty();
        let can_queue = has_text && self.is_streaming(&session);
        match (can_queue, secondary, self.follow_up_behavior()) {
            (true, false, FollowUpBehavior::Queue) => self.queue_composer(window, cx),
            (true, false, FollowUpBehavior::Steer) => {
                self.send_now(Some(Delivery::Steer), window, cx)
            }
            _ => self.send_now(None, window, cx),
        }
    }

    /// Whether the selected session can take a new send identity now.
    fn can_start_send(&self, session: &SessionId) -> bool {
        let key = self.mutation_key(session);
        self.persistence_ready()
            && self.prompt_allowed(&key)
            && self.send_attempts.can_send(&key)
            && self.creations.get(&key).is_none()
    }

    pub(crate) fn queue_preview_pending(&self, session: &SessionId) -> bool {
        self.queue
            .taking
            .iter()
            .any(|key| key.runtime == self.runtime && &key.session == session)
    }

    /// Queues the composer's text with the selection it would send with
    /// (`handleQueueMessage`). The composer clears at once; a failure gives
    /// the text back after anything typed since.
    pub(crate) fn queue_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.selected_queue_key() else {
            return;
        };
        if !self.can_start_send(&key.session) || self.queue_preview_pending(&key.session) {
            return;
        }
        let Some(owner) = self.composer_key.clone().filter(|owner| {
            owner.runtime() == &key.runtime
                && owner.target() == &DraftTarget::Session(key.session.clone())
        }) else {
            return;
        };
        let Some(model) = self.send_selection().and_then(|(_, selection)| {
            selection
                .model
                .clone()
                .map(|model| (model, selection.agent_name.clone()))
        }) else {
            return;
        };
        let Some((client, _)) = self.queue_client() else {
            return;
        };
        let text = self.composer.read(cx).value().trim_matches('\n').to_owned();
        if text.trim().is_empty() {
            return;
        }
        let (model, agent) = model;
        let item = QueueItemInput {
            content: text.clone(),
            text: text.clone(),
            send_config: QueuedSendConfig {
                provider_id: model.provider_id,
                model_id: model.model_id,
                agent,
                variant: model.variant,
            },
        };
        self.composer
            .update(cx, |composer, cx| composer.set_value("", window, cx));
        self.save_composer_draft(cx);
        self.queue_persistence(cx);
        let request = {
            let session = key.session.clone();
            let directory = key.directory.clone();
            let connection = self
                .connection
                .as_ref()
                .expect("a client implies a connection");
            connection
                .network
                .run(async move { client.enqueue_message(&session, &directory, &item).await })
        };
        let generation = self.queue.generation;
        self.queue_task(|id| {
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.queue.tasks.remove(&id);
                    if generation != this.queue.generation {
                        // The text still belongs to its draft owner.
                        if !matches!(result, Some(Ok(_))) {
                            this.give_back_text(&owner, text, window, cx);
                        }
                        return;
                    }
                    match result {
                        Some(Ok(update)) => this.settle_queue_mutation(&key, Some(Ok(update)), cx),
                        failure => {
                            tracing::warn!(error = ?failure.and_then(Result::err), "queueing failed");
                            this.give_back_text(&owner, text, window, cx);
                            this.notify_queue_failure("chat.queuedMessage.toast.queueFailed", window, cx);
                            cx.notify();
                        }
                    }
                });
            })
        });
        cx.notify();
    }

    /// Removes a queued message (`removeFromQueue`): at once on screen, then
    /// on the server, whose answer or a reread replaces the queue.
    pub(crate) fn remove_queued(&mut self, id: &QueuedItemId, cx: &mut Context<Self>) {
        let Some(key) = self.selected_queue_key() else {
            return;
        };
        let Some((client, _)) = self.queue_client() else {
            return;
        };
        if !self.queue.projection.remove_local(&key, id) {
            return;
        }
        let (session, directory, item) = (key.session.clone(), key.directory.clone(), id.clone());
        let request = self
            .connection
            .as_ref()
            .expect("a client implies a connection")
            .network
            .run(async move { client.remove_queued(&session, &directory, &item).await });
        self.spawn_queue_mutation(key, request, cx);
        cx.notify();
    }

    /// Moves `from` to where `to` is (`reorderQueue`).
    pub(crate) fn reorder_queued(
        &mut self,
        from: &QueuedItemId,
        to: &QueuedItemId,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.selected_queue_key() else {
            return;
        };
        let Some((client, _)) = self.queue_client() else {
            return;
        };
        let Some(order) = self.queue.projection.reorder_local(&key, from, to) else {
            return;
        };
        let (session, directory) = (key.session.clone(), key.directory.clone());
        let request = self
            .connection
            .as_ref()
            .expect("a client implies a connection")
            .network
            .run(async move { client.reorder_queue(&session, &directory, &order).await });
        self.spawn_queue_mutation(key, request, cx);
        cx.notify();
    }

    fn spawn_queue_mutation(
        &mut self,
        key: QueueKey,
        request: crate::network::NetworkTask<Result<QueueUpdate, TransportError>>,
        cx: &mut Context<Self>,
    ) {
        let generation = self.queue.generation;
        self.queue_task(|id| {
            cx.spawn(async move |this, cx| {
                let result = request.await;
                let _ = this.update(cx, |this, cx| {
                    this.queue.tasks.remove(&id);
                    if generation == this.queue.generation {
                        this.settle_queue_mutation(&key, result, cx);
                    }
                });
            })
        });
    }

    /// Edit takes the message out of the queue and puts its text in the
    /// composer, after anything already typed there, so sending it again
    /// queues one message and not a copy (`handleQueuedMessageEdit`).
    pub(crate) fn edit_queued(
        &mut self,
        id: &QueuedItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.take_queued(id, TakePurpose::Edit, window, cx);
    }

    /// Sends one queued message now, steering it in while the session works
    /// (`handleQueuedMessageSend`), with the selection it was queued with.
    pub(crate) fn send_queued(
        &mut self,
        id: &QueuedItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        if !self.can_start_send(&session) {
            return;
        }
        self.take_queued(id, TakePurpose::Send, window, cx);
    }

    /// The newest queued message, for ArrowUp in an empty composer.
    pub(crate) fn newest_editable_queued(&self) -> Option<QueuedItemId> {
        self.selected_queue()
            .last()
            .filter(|message| message.editable_as_text())
            .map(|message| message.id.clone())
    }

    fn take_queued(
        &mut self,
        id: &QueuedItemId,
        purpose: TakePurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.selected_queue_key() else {
            return;
        };
        let Some(message) = self
            .queue
            .projection
            .queue(&key)
            .iter()
            .find(|message| &message.id == id)
        else {
            return;
        };
        if !message.takeable(MAX_URI)
            || matches!(purpose, TakePurpose::Edit) && !message.editable_as_text()
            || self.queue.projection.sending(&key) == Some(id)
        {
            return;
        }
        let Some(owner) = self.session_draft_key(&key) else {
            return;
        };
        let Some((client, executor)) = self.queue_client() else {
            return;
        };
        if !self.queue.taking.insert(key.clone()) {
            return;
        }
        let (session, directory, item) = (key.session.clone(), key.directory.clone(), id.clone());
        let request = self
            .connection
            .as_ref()
            .expect("a client implies a connection")
            .network
            .run(async move { client.take_queued(&session, &directory, &item).await });
        let generation = self.queue.generation;
        self.queue_task(|task_id| {
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.queue.tasks.remove(&task_id);
                    if generation != this.queue.generation {
                        // A taken message still belongs to its draft owner.
                        if let Some(Ok((_, taken))) = result {
                            this.give_back_text(&owner, taken.message.content, window, cx);
                        }
                        return;
                    }
                    this.queue.taking.remove(&key);
                    let taken = match result {
                        Some(Ok((update, taken))) => {
                            this.settle_queue_mutation(&key, Some(Ok(update)), cx);
                            taken
                        }
                        failure => {
                            tracing::warn!(error = ?failure.and_then(Result::err), "taking a queued message failed");
                            this.read_queue_owner(key.owner().map(str::to_owned), cx);
                            this.notify_queue_failure("chat.queuedMessage.toast.takeFailed", window, cx);
                            cx.notify();
                            return;
                        }
                    };
                    match purpose {
                        TakePurpose::Edit => {
                            this.give_back_text(&owner, taken.message.content, window, cx);
                            this.focus_composer(window, cx);
                        }
                        TakePurpose::Send => {
                            let busy = this.is_streaming(&key.session);
                            // A queued-only send leaves open requests alone.
                            let outgoing = this.outgoing_from(
                                vec![taken],
                                None,
                                None,
                                busy.then_some(Delivery::Steer),
                                false,
                            );
                            this.send_taken(&key, owner, outgoing, (client_of(this), executor), window, cx);
                        }
                    }
                    cx.notify();
                });
            })
        });
        cx.notify();
    }

    /// Sends what the composer holds now together with the queued messages
    /// this client may take, the queued ones first (`handleSubmit` taking the
    /// queue): Ctrl+Enter, Enter in an idle session or on an empty composer,
    /// and Enter in steer mode with `Some(Steer)`. Goes through while the
    /// session works. The server reserves the eligible prefix atomically and
    /// keeps its payloads until the stable prompt ID is observed admitted.
    /// Goal, scheduled and unjournalable items end the prefix. A branching
    /// command in the composer never reserves the queue.
    pub(crate) fn send_now(
        &mut self,
        delivery: Option<Delivery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let composer = self.composer.read(cx).value().to_string();
        let Some(key) = self
            .selected_queue_key()
            .filter(|_| crate::session_branching::parse_command(&composer).is_none())
        else {
            self.submit(window, cx);
            return;
        };
        let ids = take_prefix(
            self.queue.projection.queue(&key),
            self.queue.projection.sending(&key),
        );
        let busy = self.is_streaming(&session);
        if ids.is_empty() {
            match delivery {
                Some(Delivery::Steer) => {
                    self.submit_connected_with(Some(Delivery::Steer), window, cx)
                }
                _ if busy && !composer.trim().is_empty() => self.submit_connected_now(window, cx),
                _ => self.submit(window, cx),
            }
            return;
        }
        if !self.can_start_send(&session) {
            return;
        }
        let Some(owner) = self.composer_key.clone().filter(|owner| {
            owner.runtime() == &key.runtime
                && owner.target() == &DraftTarget::Session(session.clone())
        }) else {
            return;
        };
        let Some(selection) = self.send_selection().map(|(_, selection)| selection) else {
            return;
        };
        let Some(model) = selection.model else {
            return;
        };
        let input = QueueItemInput {
            content: composer.clone(),
            text: composer.clone(),
            send_config: QueuedSendConfig {
                provider_id: model.provider_id,
                model_id: model.model_id,
                variant: model.variant,
                agent: selection.agent_name,
            },
        };
        let requested_id = self.message_ids.next().0;
        let Some((client, executor)) = self.queue_client() else {
            return;
        };
        if !self.queue.taking.insert(key.clone()) {
            return;
        }
        let (session_id, directory) = (key.session.clone(), key.directory.clone());
        let request = self
            .connection
            .as_ref()
            .expect("a client implies a connection")
            .network
            .run(async move {
                client
                    .preview_queued_prefix(
                        &session_id,
                        &directory,
                        &requested_id,
                        &ids,
                        &input,
                        delivery == Some(Delivery::Steer),
                    )
                    .await
            });
        let generation = self.queue.generation;
        self.queue_task(|task_id| {
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.queue.tasks.remove(&task_id);
                    if generation != this.queue.generation {
                        // The captured owner's persisted reservation retains all payloads.
                        return;
                    }
                    this.queue.taking.remove(&key);
                    let Ok(receipt) = reservation_result(result) else {
                        // None is an unknown outcome. A later reserve recovers the
                        // same receipt; no message has left the server's queue.
                        this.read_queue_owner(key.owner().map(str::to_owned), cx);
                        this.notify_queue_failure(
                            "chat.queuedMessage.toast.takeFailed",
                            window,
                            cx,
                        );
                        cx.notify();
                        return;
                    };
                    let prompt = receipt.prompt();
                    this.settle_queue_mutation(&key, Some(Ok(receipt.update)), cx);
                    if receipt.items.is_empty() {
                        return;
                    }
                    let intent = crate::send_attempt::QueueReservation {
                        items: receipt
                            .items
                            .iter()
                            .map(|item| item.message.id.to_string())
                            .collect(),
                        composer: receipt.composer.clone(),
                    };
                    // The capture is distinct from merged prompt text. The
                    // ordinary durable-send barrier clears only this capture,
                    // preserving anything typed during preview or persistence.
                    let outgoing = Outgoing {
                        draft: Draft {
                            text: composer.clone(),
                            selection: composer.len()..composer.len(),
                        },
                        text: prompt.text,
                        parts: prompt.parts,
                        model: prompt.model,
                        agent: prompt.agent,
                        delivery: prompt.delivery,
                        consumes_composer: composer == intent.composer,
                        supersedes: true,
                        reserved_id: Some((receipt.id, intent)),
                    };
                    this.send_taken(
                        &key,
                        owner,
                        outgoing,
                        (client_of(this), executor),
                        window,
                        cx,
                    );
                    cx.notify();
                });
            })
        });
        cx.notify();
    }

    /// One prompt from taken messages and an optional composer text
    /// (`buildOutgoingMessage`): their texts joined by a blank line, their
    /// files, the first agent mention, and their context in order. Without a
    /// `selection`, the first message's captured selection is used.
    fn outgoing_from(
        &mut self,
        taken: Vec<TakenMessage>,
        composer: Option<String>,
        selection: Option<(Option<ModelChoice>, Option<String>)>,
        delivery: Option<Delivery>,
        supersedes: bool,
    ) -> Outgoing {
        let mut parts = PromptParts::default();
        let mut texts: Vec<String> = Vec::new();
        for message in &taken {
            if let Some(name) = message.message.agent_mention.as_ref()
                && parts.agents.is_empty()
            {
                parts.agents.push(AgentMention {
                    name: name.clone(),
                    mention: None,
                });
            }
            if !message.message.text.trim().is_empty() {
                texts.push(message.message.text.clone());
            }
            parts.files.extend(message.files.iter().cloned());
            let ids = &mut self.message_ids;
            parts
                .context
                .extend(message.synthetic_context(|| -> MessageId { ids.next().0 }));
        }
        if let Some(composer) = composer {
            texts.push(composer);
        }
        let (model, agent) = selection.unwrap_or_else(|| {
            let config = taken
                .first()
                .and_then(|message| message.message.send_config.clone());
            (
                config.as_ref().map(|config| ModelChoice {
                    provider_id: config.provider_id.clone(),
                    model_id: config.model_id.clone(),
                    variant: config.variant.clone(),
                }),
                config.and_then(|config| config.agent),
            )
        });
        // A queue merge retains delivered text for recovery. Reserved merges
        // keep their server ownership and stable identity after any failure.
        let text = texts.join("\n\n");
        Outgoing {
            draft: Draft {
                selection: text.len()..text.len(),
                text: text.clone(),
            },
            text,
            parts,
            model,
            agent,
            delivery,
            consumes_composer: false,
            supersedes,
            reserved_id: None,
        }
    }

    /// Sends taken messages, or gives their text back to the composer when
    /// the session cannot take a send now.
    fn send_taken(
        &mut self,
        key: &QueueKey,
        owner: DraftKey,
        outgoing: Outgoing,
        client: (
            Option<openchamber_native_transport::client::ServerClient>,
            tokio::runtime::Handle,
        ),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Nothing to say: no request, and nothing to give back.
        if outgoing.text.trim().is_empty()
            && outgoing.parts.files.is_empty()
            && outgoing.parts.context.is_empty()
        {
            return;
        }
        let session_key = self.mutation_key(&key.session);
        let text = outgoing.draft.text.clone();
        let reserved = outgoing.reserved_id.is_some();
        // The projection bounded every reference; a take that disagrees is
        // not journaled and its text goes back.
        if outgoing
            .parts
            .files
            .iter()
            .any(|file| file.uri.len() > MAX_URI)
        {
            if !reserved {
                self.give_back_text(&owner, text, window, cx);
            }
            self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
            return;
        }
        let sent = match client {
            (Some(client), executor)
                if key.runtime == self.runtime && self.can_start_send(&key.session) =>
            {
                self.send_outgoing(
                    session_key,
                    owner.clone(),
                    outgoing,
                    (client, executor),
                    window,
                    cx,
                )
            }
            _ => false,
        };
        if !sent {
            if !reserved {
                self.give_back_text(&owner, text, window, cx);
            }
            self.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
        }
    }
}

/// The leading journalable prefix. An in-flight item or any unsupported
/// item ends it. The server checks these identities atomically at reservation.
fn take_prefix(queue: &[QueuedMessage], sending: Option<&QueuedItemId>) -> Vec<QueuedItemId> {
    if sending.is_some() {
        return Vec::new();
    }
    queue
        .iter()
        .take_while(|message| message.takeable(MAX_URI))
        .map(|message| message.id.clone())
        .collect()
}

fn reservation_result(
    result: Option<Result<ReservedPrefix, TransportError>>,
) -> Result<ReservedPrefix, TransportError> {
    result.unwrap_or(Err(TransportError::Cancelled))
}

/// Acquires the server prefix only after the complete request is journaled.
/// A mismatched receipt cannot change a previously captured submission.
pub(crate) async fn reserve_captured_prefix(
    client: &openchamber_native_transport::client::ServerClient,
    session: &SessionId,
    directory: &str,
    prompt: &openchamber_native_transport::prompt::Prompt,
    intent: &crate::send_attempt::QueueReservation,
) -> Result<(), TransportError> {
    let model = prompt.model.as_ref().ok_or_else(|| {
        TransportError::Malformed("a queue reservation has no captured model".into())
    })?;
    let input = QueueItemInput {
        content: intent.composer.clone(),
        text: intent.composer.clone(),
        send_config: QueuedSendConfig {
            provider_id: model.provider_id.clone(),
            model_id: model.model_id.clone(),
            variant: model.variant.clone(),
            agent: prompt.agent.clone(),
        },
    };
    let ids: Vec<_> = intent
        .items
        .iter()
        .cloned()
        .map(QueuedItemId::new)
        .collect();
    let receipt = client
        .reserve_queued_prefix(
            session,
            directory,
            &prompt.message_id,
            &ids,
            &input,
            prompt.delivery == Some(Delivery::Steer),
        )
        .await?;
    let observed = receipt.prompt();
    if receipt.composer != intent.composer
        || observed.message_id != prompt.message_id
        || observed.text != prompt.text
        || observed.parts != prompt.parts
        || observed.model != prompt.model
        || observed.agent != prompt.agent
        || observed.delivery != prompt.delivery
    {
        return Err(TransportError::Malformed(
            "queue reservation changed the captured prompt".into(),
        ));
    }
    Ok(())
}

fn client_of(workspace: &Workspace) -> Option<openchamber_native_transport::client::ServerClient> {
    workspace
        .connection
        .as_ref()
        .filter(|connection| matches!(connection.state, ConnectionState::Connected))
        .and_then(|connection| connection.client.clone())
}

#[cfg(test)]
mod reservation_tests {
    use super::*;

    #[test]
    fn a_cancelled_reservation_is_unknown_not_empty_success() {
        assert!(matches!(
            reservation_result(None),
            Err(TransportError::Cancelled)
        ));
        for status in [404, 409, 500] {
            assert!(
                matches!(reservation_result(Some(Err(TransportError::Http { status }))), Err(TransportError::Http { status: actual }) if actual == status)
            );
        }
    }

    #[test]
    fn reservation_stops_at_every_ineligible_barrier() {
        let make = |id: &str| QueuedMessage {
            id: QueuedItemId::new(id),
            content: id.into(),
            text: id.into(),
            agent_mention: None,
            attachments: Vec::new(),
            has_context: false,
            context_preview: None,
            created_at: 1,
            send_config: None,
            goal: false,
            scheduled: false,
        };
        for barrier in 0..3 {
            let mut middle = make("B");
            match barrier {
                0 => middle.goal = true,
                1 => middle.scheduled = true,
                _ => middle.attachments.push(
                    openchamber_native_core::message_queue::QueuedAttachment {
                        filename: "large".into(),
                        source: openchamber_native_core::message_queue::AttachmentSource::Local,
                        size: 100_000,
                        server_path: None,
                    },
                ),
            }
            let queue = [make("A"), middle, make("C")];
            assert_eq!(take_prefix(&queue, None), [QueuedItemId::new("A")]);
            assert!(take_prefix(&queue, Some(&queue[0].id)).is_empty());
        }
    }
}
