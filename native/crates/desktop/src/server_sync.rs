//! The workspace's connected behavior: the connection sequence, global
//! session and topology reads, history reads and live event delivery.
//!
//! Follows the connection sequence in `docs/rust-desktop/ARCHITECTURE.md`.
//! The event stream subscribes first; the global reads start once it is live,
//! so events during a read reconcile against the read's baseline instead of
//! falling between the two. Every result checks the connection generation
//! that requested it and is dropped when a newer attempt replaced it.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::{Context, ListOffset, Window, px};
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_core::ordering::{ActivityPhase, OrderingMutation};
use openchamber_native_core::ownership::ProjectWorktrees;
use openchamber_native_core::scroll_memory::{ScrollKey, ScrollPosition};
use openchamber_native_core::session::{LiveStatus, SessionRecord};
use openchamber_native_core::session_index::{IndexUpdate, SessionIndex};
use openchamber_native_core::tool_names;
use openchamber_native_core::transcript::{
    PartContent, ToolStatus, TranscriptChange, TranscriptEvent,
};
use openchamber_native_transport::client::{MessagePage, ServerClient, SettingsChanges};
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::ServerEvent;
use openchamber_native_transport::identity::{GLOBAL_EVENTS_CAPABILITY, ServerIdentity};
use openchamber_native_transport::stream::{EventStream, StreamItem};

use crate::code_card::CodeWrap;
use crate::code_wrap_state::CodeWrapState;
use crate::connection::{ConnectionFailure, ConnectionState};
use crate::diagnostics::Marker;
use crate::workspace::{History, OpenSession, Topology, Workspace};

/// Messages read when a session opens, as the web client reads.
const HISTORY_PAGE_SIZE: usize = 100;
/// Delay before retrying a server that could not be reached.
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// Global session pages a read may hold before the desktop takes them.
const PAGE_QUEUE: usize = 4;

impl Workspace {
    /// Starts a connection attempt, cancelling the previous one's work.
    pub(crate) fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let generation = connection.restart();
        let config = connection.config.clone();
        let task = connection.network.run(async move {
            let anonymous = ServerClient::new(config.endpoint.clone(), Credential::None)?;
            let identity = anonymous.probe().await?;
            let client = match &config.password {
                Some(password) => ServerClient::new(
                    config.endpoint,
                    anonymous.login(password, &config.client).await?,
                )?,
                None => anonymous,
            };
            Ok::<_, TransportError>((client.with_identity(&identity), identity))
        });
        connection.connect_task = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_connect(generation, outcome, window, cx);
            });
        }));
        cx.notify();
    }

    fn finish_connect(
        &mut self,
        generation: u64,
        outcome: Option<Result<(ServerClient, ServerIdentity), TransportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self
            .connection
            .as_mut()
            .filter(|connection| connection.generation == generation)
        else {
            return;
        };
        let outcome = match outcome {
            Some(Ok((_, identity))) if !identity.supports(GLOBAL_EVENTS_CAPABILITY) => Err(
                TransportError::Incompatible("the server has no global event bridge".to_owned()),
            ),
            Some(outcome) => outcome,
            None => Err(TransportError::Cancelled),
        };
        match outcome {
            Ok((client, identity)) => {
                tracing::info!(
                    version = identity.openchamber_version,
                    runtime = identity.runtime,
                    "connected to OpenChamber"
                );
                let runtime = RuntimeId::new(format!(
                    "{}#{}",
                    connection.config.endpoint.as_str(),
                    identity.server_id.as_deref().unwrap_or_default()
                ));
                connection.client = Some(client);
                connection.state = ConnectionState::Connected;
                if runtime != self.runtime {
                    self.switch_runtime(runtime, cx);
                }
                self.subscribe(generation, window, cx);
                self.save_code_wrap(cx);
            }
            Err(error) => {
                let failure = ConnectionFailure::from_error(&error);
                tracing::warn!(%error, "the server connection failed");
                connection.state = ConnectionState::Failed(failure);
                if failure.is_transient() {
                    connection.retry_task = Some(cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(RETRY_DELAY).await;
                        let _ = this.update_in(cx, |this, window, cx| this.connect(window, cx));
                    }));
                }
            }
        }
        cx.notify();
    }

    /// Replaces every runtime-scoped record. Drafts and scroll positions keep
    /// their runtime keys, so they never restore into another server.
    fn switch_runtime(&mut self, runtime: RuntimeId, cx: &mut Context<Self>) {
        self.code_wrap_save_task = None;
        self.code_wrap_state = CodeWrapState::default();
        cx.set_global(CodeWrap::default());
        self.runtime = runtime;
        self.sessions = SessionIndex::new();
        self.topology = Topology::default();
        self.reset_send_catalog();
        self.reset_project_readouts();
        self.open_sessions.clear();
        self.live_status.clear();
        self.selected = None;
        self.composer_key = self.new_session_draft_key();
        self.rebuild_sidebar();
    }

    fn subscribe(&mut self, generation: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let mut stream = {
            let _runtime = connection.network.enter();
            EventStream::subscribe(client)
        };
        connection.stream_task = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(item) = stream.next().await {
                let applied = this.update_in(cx, |this, window, cx| {
                    this.apply_stream_item(generation, item, window, cx);
                });
                if applied.is_err() {
                    return;
                }
            }
        }));
    }

    fn apply_stream_item(
        &mut self,
        generation: u64,
        item: StreamItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self
            .connection
            .as_mut()
            .filter(|connection| connection.generation == generation)
        else {
            return;
        };
        match item {
            StreamItem::Connected { resumed } => {
                tracing::debug!(resumed, "the event stream is live");
                let stale = std::mem::take(&mut connection.stale);
                connection.state = ConnectionState::Connected;
                if !connection.loaded {
                    connection.loaded = true;
                    self.reconcile(window, cx);
                } else if stale && !resumed {
                    // Events were missed and not replayed: reread what they changed.
                    self.reconcile(window, cx);
                }
            }
            StreamItem::Disconnected(error) => {
                tracing::warn!(%error, "the event stream dropped");
                connection.stale = true;
                connection.state = match ConnectionFailure::from_error(&error) {
                    ConnectionFailure::Unreachable => ConnectionState::Reconnecting,
                    failure => ConnectionState::Failed(failure),
                };
                // Show the server's data while the stream retries; the next
                // connection rereads what it may have missed.
                if !connection.loaded {
                    connection.loaded = true;
                    self.reconcile(window, cx);
                }
            }
            StreamItem::Events(events) => self.apply_server_events(events, window, cx),
        }
        cx.notify();
    }

    /// Rereads everything live state may have missed: sessions, topology,
    /// running state and every visited transcript.
    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        tracing::info!(
            open_sessions = self.open_sessions.len(),
            "reading sessions, topology and visited transcripts"
        );
        self.load_sessions(window, cx);
        self.load_topology(cx);
        let open: Vec<SessionId> = self.open_sessions.keys().cloned().collect();
        for id in open {
            self.load_history(&id, cx);
        }
    }

    fn load_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let generation = connection.generation;
        let ticket = self.sessions.begin_load();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(PAGE_QUEUE);
        let statuses_client = client.clone();
        let pages = connection.network.run(async move {
            let mut pages = client.session_pages();
            loop {
                let (page, last) = match pages.next().await {
                    Ok(Some(page)) => (Ok(page), false),
                    Ok(None) => return,
                    Err(error) => (Err(error), true),
                };
                if sender.send(page).await.is_err() || last {
                    return;
                }
            }
        });
        let statuses = connection
            .network
            .run(async move { statuses_client.session_statuses().await });
        connection.sessions_task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut sessions: Vec<SessionRecord> = Vec::new();
            let mut first = true;
            while let Some(page) = receiver.recv().await {
                match page {
                    Ok(page) => {
                        if std::mem::take(&mut first) {
                            let early = page.clone();
                            let _ = this.update_in(cx, |this, window, cx| {
                                let update = this.sessions.merge_page(ticket, early);
                                this.after_index_update(update, window, cx);
                            });
                        }
                        sessions.extend(page);
                    }
                    Err(error) => {
                        let _ = this.update(cx, |this, cx| {
                            tracing::warn!(%error, "the session list could not be read");
                            this.sessions.fail_load(ticket);
                            this.note_failure(generation, &error);
                            cx.notify();
                        });
                        return;
                    }
                }
            }
            let complete = pages.await.is_some();
            let statuses = statuses.await.and_then(Result::ok);
            let _ = this.update_in(cx, |this, window, cx| {
                if !complete {
                    this.sessions.fail_load(ticket);
                    cx.notify();
                    return;
                }
                let count = sessions.len();
                let update = this.sessions.complete_load(ticket, sessions);
                if !std::mem::replace(&mut this.sessions_shown, true) {
                    this.mark_on_paint(Marker::SessionsShown, None, Some(count));
                }
                this.ordering.raise_baselines(this.sessions.active());
                if let Some(statuses) = statuses {
                    this.apply_status_snapshot(statuses);
                }
                this.after_index_update(update, window, cx);
                this.rebuild_sidebar();
                cx.notify();
            });
        }));
    }

    /// Replaces running state with the host's snapshot. A failed read keeps
    /// what the live channel established.
    fn apply_status_snapshot(&mut self, statuses: HashMap<SessionId, LiveStatus>) {
        let known: Vec<SessionId> = self.live_status.keys().cloned().collect();
        self.live_status = statuses
            .into_iter()
            .filter(|(_, status)| *status != LiveStatus::Idle)
            .collect();
        let active: Vec<&SessionId> = self.live_status.keys().collect();
        self.ordering
            .reconcile_snapshot(active, known.iter(), now_ms());
    }

    /// Turns code line wrapping on or off and saves the choice as the web
    /// does. A failed save keeps the choice on screen.
    pub(crate) fn toggle_code_wrap(&mut self, cx: &mut Context<Self>) {
        let wrap = !cx.try_global::<CodeWrap>().copied().unwrap_or_default().0;
        self.show_code_wrap(wrap, cx);
        self.code_wrap_state.change(wrap);
        self.save_code_wrap(cx);
    }

    /// Drains one runtime's latest choices in order. The captured client
    /// continues across a reconnect to that runtime; changing runtimes or
    /// closing the workspace cancels this task and its network request.
    fn save_code_wrap(&mut self, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let Some(save) = self.code_wrap_state.begin_save() else {
            return;
        };
        let runtime = self.runtime.clone();
        let generation = connection.generation;
        let changes = SettingsChanges {
            code_block_line_wrap: Some(save.value),
        };
        let first_client = client.clone();
        let task = connection
            .network
            .run(async move { first_client.save_settings(&changes).await });
        self.code_wrap_save_task =
            Some(cx.spawn(async move |this, cx| {
                let mut current = Some((save, task));
                while let Some((save, task)) = current {
                    let result = task.await.unwrap_or(Err(TransportError::Cancelled));
                    current = this.update(cx, |this, cx| {
                    if this.runtime != runtime {
                        return None;
                    }
                    let latest = this.code_wrap_state.finish_save(save);
                    let same_generation = this.connection.as_ref().is_some_and(|connection| {
                        connection.generation == generation
                    });
                    match result {
                        Ok(settings) if latest && same_generation => {
                            if let Some(wrap) = settings.code_block_line_wrap {
                                this.show_code_wrap(wrap, cx);
                                if let Some(held) = this.send_catalog.settings.as_mut() {
                                    held.code_block_line_wrap = Some(wrap);
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(%error, "the code wrap setting could not be saved");
                        }
                    }
                    let save = this.code_wrap_state.begin_save()?;
                    let client = client.clone();
                    let changes = SettingsChanges {
                        code_block_line_wrap: Some(save.value),
                    };
                    let task = this.connection.as_ref()?.network.run(async move {
                        client.save_settings(&changes).await
                    });
                    Some((save, task))
                }).ok().flatten();
                }
            }));
    }

    fn load_topology(&mut self, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let generation = connection.generation;
        let wrap_revision = self.code_wrap_state.revision();
        let task = connection.network.run(async move {
            let roots = client.chat_roots().await;
            let settings = client.settings().await;
            let worktrees = match &settings {
                Ok(settings) => {
                    let paths: Vec<String> = settings
                        .projects
                        .iter()
                        .map(|project| project.path.clone())
                        .collect();
                    Some(client.project_worktrees(&paths).await)
                }
                Err(_) => None,
            };
            (roots, settings, worktrees)
        });
        connection.topology_task = Some(cx.spawn(async move |this, cx| {
            let Some((roots, settings, worktrees)) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if !this
                    .connection
                    .as_ref()
                    .is_some_and(|connection| connection.generation == generation)
                {
                    return;
                }
                // Each part fails alone and keeps what it held, so a failed
                // discovery leaves sessions reachable through their fallback.
                match roots {
                    Ok(roots) => this.topology.chat_roots = roots,
                    Err(error) => tracing::warn!(%error, "the Chats roots could not be read"),
                }
                match settings {
                    Ok(mut settings) => {
                        this.topology.worktrees.retain(|entry| {
                            settings
                                .projects
                                .iter()
                                .any(|project| project.path == entry.project_path)
                        });
                        this.topology.projects = settings.projects.clone();
                        if this.code_wrap_state.can_adopt(wrap_revision) {
                            if let Some(wrap) = settings.code_block_line_wrap {
                                this.show_code_wrap(wrap, cx);
                            }
                        } else {
                            settings.code_block_line_wrap =
                                Some(cx.try_global::<CodeWrap>().copied().unwrap_or_default().0);
                        }
                        this.send_catalog.settings = Some(settings);
                    }
                    Err(error) => tracing::warn!(%error, "the settings could not be read"),
                }
                match worktrees {
                    Some(Ok(reads)) => {
                        for read in reads {
                            let Some(worktrees) = read.worktrees else {
                                tracing::warn!("a project's worktrees could not be listed");
                                continue;
                            };
                            let entry = ProjectWorktrees {
                                project_path: read.project_path,
                                worktrees,
                            };
                            match this
                                .topology
                                .worktrees
                                .iter_mut()
                                .find(|held| held.project_path == entry.project_path)
                            {
                                Some(held) => *held = entry,
                                None => this.topology.worktrees.push(entry),
                            }
                        }
                    }
                    Some(Err(error)) => tracing::warn!(%error, "the worktrees could not be read"),
                    None => {}
                }
                this.rebuild_sidebar();
                cx.notify();
            });
        }));
    }

    /// Reads a visited session's newest messages and merges them.
    pub(crate) fn load_history(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        // Before the connection is up, the first sync reads every visited session.
        let Some(client) = connection.client.clone() else {
            return;
        };
        let Some(open) = self.open_sessions.get_mut(id) else {
            return;
        };
        let since = open.transcript.revision();
        let requested = Instant::now();
        let request_id = id.clone();
        let task = connection
            .network
            .run(async move { client.latest_messages(&request_id, HISTORY_PAGE_SIZE).await });
        let id = id.clone();
        open.history = History::Loading {
            _read: cx.spawn(async move |this, cx| {
                let outcome = task.await.unwrap_or(Err(TransportError::Cancelled));
                let _ = this.update(cx, |this, cx| {
                    this.finish_history(&id, since, requested, outcome, cx);
                });
            }),
        };
    }

    fn finish_history(
        &mut self,
        id: &SessionId,
        since: u64,
        requested: Instant,
        outcome: Result<MessagePage, TransportError>,
        cx: &mut Context<Self>,
    ) {
        let position = self.scroll_memory.position(&ScrollKey {
            runtime: self.runtime.clone(),
            session: id.clone(),
        });
        let position = position.clone();
        let Some(open) = self.open_sessions.get_mut(id) else {
            return;
        };
        match outcome {
            Ok(page) => {
                let was_empty = open.transcript.is_empty();
                let change = open.transcript.materialize(page.messages, since);
                open.older = page.older;
                open.history = History::Complete;
                apply_list_change(open, &change);
                if was_empty {
                    restore_position(open, &position);
                    let count = open.transcript.len();
                    self.mark_on_paint(Marker::HistoryShown, Some(requested), Some(count));
                }
            }
            Err(error) => {
                tracing::warn!(%error, "a session's history could not be read");
                open.history = History::Failed(ConnectionFailure::from_error(&error));
            }
        }
        cx.notify();
    }

    fn apply_server_events(
        &mut self,
        events: Vec<ServerEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut changes = Vec::new();
        let mut ordering = Vec::new();
        let mut reread: Vec<SessionId> = Vec::new();
        let mut git_changed: Vec<SessionId> = Vec::new();
        for event in events {
            if let Some(change) = event.session_change() {
                changes.push(change);
            }
            match event {
                ServerEvent::SessionStatus { session_id, status } => {
                    let phase = match status {
                        LiveStatus::Busy | LiveStatus::Retry => ActivityPhase::Active,
                        LiveStatus::Idle => ActivityPhase::Settled,
                    };
                    if status == LiveStatus::Idle {
                        self.live_status.remove(&session_id);
                    } else {
                        self.live_status.insert(session_id.clone(), status);
                    }
                    ordering.push(OrderingMutation::Observe { session_id, phase });
                }
                ServerEvent::SessionDeleted { session_id } => {
                    self.live_status.remove(&session_id);
                    ordering.push(OrderingMutation::Remove { session_id });
                }
                ServerEvent::CatalogUpdated { kind, directory } => {
                    self.refresh_send_catalog(kind, directory.as_deref(), cx);
                }
                ServerEvent::Transcript { session_id, event } => {
                    if self.finishes_git_changing_call(&session_id, &event)
                        && !git_changed.contains(&session_id)
                    {
                        git_changed.push(session_id.clone());
                    }
                    if self.apply_transcript_event(&session_id, event)
                        && !reread.contains(&session_id)
                    {
                        reread.push(session_id);
                    }
                }
                _ => {}
            }
        }
        let update = self.sessions.apply(changes);
        let reordered = !ordering.is_empty() && self.ordering.apply(&ordering, now_ms());
        if reordered && !update.changed {
            self.rebuild_sidebar();
        }
        self.after_index_update(update, window, cx);
        for id in reread {
            self.load_history(&id, cx);
        }
        for id in git_changed {
            if let Some(directory) = self
                .sessions
                .get(&id)
                .and_then(|session| session.resolved_directory())
            {
                self.refresh_git_status_soon(&directory, cx);
            }
        }
    }

    /// Whether `event` finishes a call of a tool that may change what Git
    /// reports (`requestGitRefreshForToolTransition`): its first move to a
    /// final state.
    fn finishes_git_changing_call(&self, id: &SessionId, event: &TranscriptEvent) -> bool {
        let TranscriptEvent::ToolTransition {
            message_id,
            part_id,
            status: ToolStatus::Completed | ToolStatus::Error,
            ..
        } = event
        else {
            return false;
        };
        let Some(open) = self.open_sessions.get(id) else {
            return false;
        };
        open.transcript
            .row_of(message_id)
            .and_then(|row| open.transcript.message(row))
            .and_then(|message| message.parts.iter().find(|part| &part.id == part_id))
            .is_some_and(|part| {
                matches!(
                    &part.content,
                    PartContent::Tool { tool, status, .. }
                        if !matches!(status, ToolStatus::Completed | ToolStatus::Error)
                            && tool_names::may_change_git(tool)
                )
            })
    }

    /// Applies a live transcript event to a visited session. Returns whether
    /// the session needs an authoritative read.
    fn apply_transcript_event(&mut self, id: &SessionId, event: TranscriptEvent) -> bool {
        let Some(open) = self.open_sessions.get_mut(id) else {
            return false;
        };
        let change = open.transcript.apply(event);
        if change == TranscriptChange::NeedsReconcile {
            // A read in flight covers what the event referred to.
            return !matches!(open.history, History::Loading { .. });
        }
        apply_list_change(open, &change);
        false
    }

    /// Rebuilds the sidebar after an index change and keeps the selection on
    /// a session that is still listed.
    fn after_index_update(
        &mut self,
        update: IndexUpdate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !update.moves.is_empty() {
            // A move can land in a worktree this client has not discovered.
            self.load_topology(cx);
        }
        if !update.changed {
            return;
        }
        self.rebuild_sidebar();
        // A selected session that left the list returns the window to the
        // new-session view rather than opening an unrelated session.
        let selection_gone = self
            .selected
            .as_ref()
            .is_some_and(|id| self.session(id).is_none());
        if selection_gone {
            self.show_new_session(window, cx);
        }
        cx.notify();
    }

    /// Records a failure that makes the connection unusable.
    fn note_failure(&mut self, generation: u64, error: &TransportError) {
        if let Some(connection) = self
            .connection
            .as_mut()
            .filter(|connection| connection.generation == generation)
        {
            let failure = ConnectionFailure::from_error(error);
            if !failure.is_transient() {
                connection.state = ConnectionState::Failed(failure);
            }
        }
    }
}

/// Mirrors a transcript change in the list's measured rows.
pub(crate) fn apply_list_change(open: &OpenSession, change: &TranscriptChange) {
    match change {
        TranscriptChange::Inserted { rows } => open.list.splice(rows.start..rows.start, rows.len()),
        TranscriptChange::Updated { row } => open.list.remeasure_items(*row..row + 1),
        TranscriptChange::Removed { rows } => open.list.splice(rows.clone(), 0),
        TranscriptChange::Reset => {
            let following = open.list.is_following_tail();
            open.list.reset(open.transcript.len());
            if following {
                open.list.scroll_to_end();
            }
        }
        TranscriptChange::Unchanged | TranscriptChange::NeedsReconcile => {}
    }
}

/// Restores a remembered reading position once the transcript has rows.
fn restore_position(open: &OpenSession, position: &ScrollPosition) {
    let anchor = match position {
        ScrollPosition::Anchored {
            message_id,
            offset_in_message,
        } => open
            .transcript
            .row_of(message_id)
            .map(|item_ix| (item_ix, *offset_in_message)),
        ScrollPosition::FollowingTail => None,
    };
    match anchor {
        Some((item_ix, offset)) => open.list.scroll_to(ListOffset {
            item_ix,
            offset_in_item: px(offset),
        }),
        None => open.list.scroll_to_end(),
    }
}

/// Wall-clock milliseconds, for ordering live transitions.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}
