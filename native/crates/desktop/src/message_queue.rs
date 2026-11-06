//! The composer's side of the server-owned message queue: the projection
//! the queued strip draws, the reads that fill it, and the queue, edit,
//! send, remove and reorder actions, as `ChatInput.tsx` and
//! `QueuedMessagesStrip.tsx` drive `stores/messageQueueStore.ts`.
//!
//! The server delivers the queue when the session goes idle; nothing here
//! sends a queued message on its own. A queued message the user sends or
//! edits is taken out of the queue first, so it can never go out twice.
//! Payload-backed takes and explicit queued sends live in queue_take. Their
//! original seals and local destinations survive uncertain outcomes.

use std::collections::{HashMap, HashSet};

use gpui_kit::{Context, Task, Window};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::message_queue::{
    FollowUpBehavior, MessageQueue, QueueKey, QueuedItemId, QueuedMessage,
};
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::message_queue::QueueUpdate;
use openchamber_native_transport::prompt::Delivery;

use crate::connection::ConnectionState;
use crate::workspace::Workspace;

/// The queue projection and the work in flight on it. Runtime-scoped: a
/// runtime switch drops obsolete projection completions. Durable enqueue
/// chains retain their captured owners independently of that projection.
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
    /// Durable enqueue chains retain their original host across disk waits.
    pub(crate) enqueues: crate::queue_enqueue::EnqueueState,
    pub(crate) takes: crate::queue_take::TakeState,
    pub(crate) prefixes: crate::queue_prefix::PrefixState,
    pub(crate) local_sends: crate::queue_local_send::LocalSendState,
}

impl QueueState {
    pub(crate) fn any_payload_busy(&self) -> bool {
        self.enqueues.any_busy()
            || self.takes.any_busy()
            || self.prefixes.any_busy()
            || self.local_sends.any_busy()
    }
}

/// What a take is for once the server answered with the message.
pub(crate) enum TakePurpose {
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
        if self
            .composer_key
            .as_ref()
            .is_some_and(|owner| self.attachment_storage.has_pending(owner))
        {
            return;
        }
        if self.run_branch_command(window, cx) {
            return;
        }
        if self.btw_takes_composer() {
            self.send_btw_follow_up(window, cx);
            return;
        }
        if !self.session_references_ready(
            Box::new(move |this, window, cx| this.submit_from_enter(secondary, window, cx)),
            window,
            cx,
        ) {
            return;
        }
        let has_content = self.composer_has_sendable_content(cx);
        let can_queue = has_content && self.is_streaming(&session);
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
            && !self.payload_take_blocks_session(&self.runtime, session)
            && !self.payload_prefix_blocks_session(&self.runtime, session)
            && !self.payload_local_send_blocks_session(&self.runtime, session)
            && self.prompt_allowed(&key)
            && self.send_attempts.can_send(&key)
            && self.creations.get(&key).is_none()
    }

    pub(crate) fn queue_preview_pending(&self, session: &SessionId) -> bool {
        self.payload_take_blocks_session(&self.runtime, session)
            || self.payload_prefix_blocks_session(&self.runtime, session)
            || self.payload_local_send_blocks_session(&self.runtime, session)
            || self.payload_enqueue_blocks_session(&self.runtime, session)
    }

    /// Enqueue-only gate for actors that must exclude their own send operation.
    pub(crate) fn payload_enqueue_blocks_session(
        &self,
        runtime: &openchamber_native_core::ids::RuntimeId,
        session: &SessionId,
    ) -> bool {
        self.queue.enqueues.blocks_session(runtime, session)
            || self.queue_storage.operations().iter().any(|operation| {
                operation.owner().runtime() == runtime.as_str()
                    && operation.owner().session() == session.as_str()
                    && matches!(
                        operation.action(),
                        crate::queue_storage::QueueAction::Enqueue {
                            phase: crate::queue_storage::QueueEnqueuePhase::Capturing
                                | crate::queue_storage::QueueEnqueuePhase::Ready { .. }
                                | crate::queue_storage::QueueEnqueuePhase::Unknown { .. },
                            ..
                        }
                    )
            })
    }

    /// Captures the complete composer before the payload-backed enqueue starts.
    pub(crate) fn queue_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .selected
            .as_ref()
            .is_some_and(|session| self.can_start_send(session))
        {
            self.begin_payload_enqueue(window, cx);
        }
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
        if self
            .selected
            .as_ref()
            .is_some_and(|session| self.payload_local_send_blocks_session(&self.runtime, session))
        {
            return;
        }
        self.begin_payload_take(id, TakePurpose::Edit, window, cx);
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
        self.begin_payload_take(id, TakePurpose::Send, window, cx);
    }

    /// The newest queued message, for ArrowUp in an empty composer.
    pub(crate) fn newest_editable_queued(&self) -> Option<QueuedItemId> {
        self.selected_queue()
            .last()
            .filter(|message| message.payload_takeable())
            .map(|message| message.id.clone())
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
        if self.payload_local_send_blocks_session(&self.runtime, &session) {
            return;
        }
        let composer = self.composer.read(cx).value().to_string();
        let Some(key) = self
            .selected_queue_key()
            .filter(|_| crate::session_branching::parse_command(&composer).is_none())
        else {
            self.submit(window, cx);
            return;
        };
        let ids = crate::queue_prefix::leading_ids(
            self.queue.projection.queue(&key),
            self.queue.projection.sending(&key),
        );
        let busy = self.is_streaming(&session);
        if ids.is_empty() {
            match delivery {
                Some(Delivery::Steer) => {
                    self.submit_connected_with(Some(Delivery::Steer), window, cx)
                }
                _ if busy && self.composer_has_sendable_content(cx) => {
                    self.submit_connected_now(window, cx)
                }
                _ => self.submit(window, cx),
            }
            return;
        }
        if !self.can_start_send(&session) {
            return;
        }
        self.begin_payload_prefix(key, ids, delivery, window, cx);
    }
}
