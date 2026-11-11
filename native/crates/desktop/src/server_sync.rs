//! The workspace's connected behavior: the connection sequence, global
//! session and topology reads, history reads and live event delivery.
//!
//! Follows the connection sequence in `docs/rust-desktop/ARCHITECTURE.md`.
//! The event stream subscribes first; the global reads start once it is live,
//! so events during a read reconcile against the read's baseline instead of
//! falling between the two. Every result checks the connection generation
//! that requested it and is dropped when a newer attempt replaced it.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::{Context, ListOffset, Window, px};
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_core::ordering::{ActivityPhase, OrderingMutation};
use openchamber_native_core::ownership::ProjectWorktrees;
use openchamber_native_core::scroll_memory::{ScrollKey, ScrollPosition};
use openchamber_native_core::session::{LiveStatus, SessionRecord};
use openchamber_native_core::session_activity::SessionActivity;
use openchamber_native_core::session_index::{IndexUpdate, SessionIndex};
use openchamber_native_core::tool_names;
use openchamber_native_core::transcript::{
    PartContent, ToolStatus, TranscriptChange, TranscriptEvent,
};
use openchamber_native_transport::client::{MessagePage, SettingsChanges};
use openchamber_native_transport::control_stream::{ControlEvent, ControlItem, ControlStream};
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::{ExecutionEdge, ServerEvent};
use openchamber_native_transport::stream::{EventStream, StreamItem};

use crate::authentication::{ConnectOutcome, RememberedCredential, connect_server};
use crate::code_card::CodeWrap;
use crate::code_wrap_state::CodeWrapState;
use crate::connection::{ConnectionFailure, ConnectionState};
use crate::credential_store::CredentialOwner;
use crate::diagnostics::Marker;
use crate::send_attempt::HistoryMerge;
use crate::workspace::{EarlierHistory, History, OpenSession, Topology, Workspace};

/// Messages read when a session opens, as the web client reads.
const HISTORY_PAGE_SIZE: usize = 100;
/// Delay before retrying a server that could not be reached.
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// Global session pages a read may hold before the desktop takes them.
const PAGE_QUEUE: usize = 4;

#[derive(Clone, Copy)]
enum SessionDeletionCleanup {
    CurrentSubtree,
    CapturedSession,
}

struct HistoryRead {
    session: SessionId,
    runtime: RuntimeId,
    connection_generation: u64,
    generation: u64,
    since: u64,
    requested: Instant,
}

impl Workspace {
    /// Starts a connection attempt, cancelling the previous one's work.
    pub(crate) fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_session_admission(window, cx);
        for open in self.open_sessions.values_mut() {
            open.earlier = EarlierHistory::Idle;
        }
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let generation = connection.restart();
        let config = connection.take_attempt_config();
        let target_endpoint = config.endpoint.clone();
        let lease = self.authentication.store.capture(config.endpoint.as_str());
        let task = connection.network.run(connect_server(
            config,
            self.authentication.remembered.clone(),
            self.authentication.store.clone(),
            lease,
        ));
        connection.connect_task = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this
                    .connection
                    .as_ref()
                    .is_none_or(|connection| connection.config.endpoint != target_endpoint)
                {
                    return;
                }
                this.finish_connect(generation, outcome, window, cx);
            });
        }));
        self.invalidate_terminal_client(cx);
        cx.notify();
    }

    pub(crate) fn finish_connect(
        &mut self,
        generation: u64,
        outcome: Option<Result<ConnectOutcome, TransportError>>,
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
        let outcome = outcome.unwrap_or(Err(TransportError::Cancelled));
        match outcome {
            Ok(ConnectOutcome::Ready(verified)) => {
                let verified = *verified;
                let requested_focus = std::mem::take(&mut connection.focus_on_ready);
                let focus_composer = self.authentication.visible || requested_focus;
                let identity = verified.identity;
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
                if let Some(owner) = CredentialOwner::new(&connection.config.endpoint, &identity)
                    && let Some(lease) = verified.lease
                    && self.authentication.store.is_current(&lease)
                {
                    self.authentication.remembered.insert(
                        connection.config.endpoint.as_str().to_owned(),
                        RememberedCredential {
                            owner,
                            credential: verified.credential,
                            lease,
                        },
                    );
                }
                self.authentication.visible = false;
                self.authentication.submitting = false;
                self.authentication.failure = None;
                self.authentication
                    .password
                    .update(cx, |input, cx| input.set_value("", window, cx));
                let auth_failures = verified.client.authentication_failures();
                connection.client = Some(verified.client);
                connection.state = ConnectionState::Connected;
                connection.established = true;
                connection.recovery = None;
                if runtime != self.runtime {
                    self.switch_runtime(runtime, window, cx);
                }
                self.invalidate_terminal_client(cx);
                self.watch_authentication(generation, auth_failures, window, cx);
                self.subscribe(generation, window, cx);
                self.subscribe_control(generation, window, cx);
                self.check_compatibility(generation, window, cx);
                self.load_server_policy(cx);
                self.save_code_wrap(cx);
                if focus_composer {
                    self.focus_composer(window, cx);
                }
            }
            Ok(ConnectOutcome::Locked {
                status,
                storage_error,
            }) => {
                let failure = match status {
                    openchamber_native_transport::authentication::AuthenticationStatus::ClientTokenRequired => ConnectionFailure::ClientAuthenticationRequired,
                    _ => ConnectionFailure::AuthenticationRequired,
                };
                connection.state = ConnectionState::Failed(failure);
                self.show_authentication(failure, storage_error, window, cx);
            }
            Err(error) => {
                let failure = ConnectionFailure::from_error(&error);
                tracing::warn!(%error, "the server connection failed");
                connection.state = ConnectionState::Failed(failure);
                if !connection.established {
                    connection.recovery = crate::connection_recovery::RecoveryVariant::of(failure);
                }
                if matches!(
                    failure,
                    ConnectionFailure::AuthenticationRequired
                        | ConnectionFailure::AuthenticationRejected
                        | ConnectionFailure::ClientAuthenticationRequired
                        | ConnectionFailure::AuthenticationRateLimited { .. }
                ) {
                    self.show_authentication(failure, None, window, cx);
                } else if failure.is_transient() {
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
    pub(crate) fn switch_runtime(
        &mut self,
        runtime: RuntimeId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The palette lists the outgoing runtime's sessions, files and client.
        self.close_command_palette(window, cx);
        self.effort_preferences.activate(&runtime);
        self.reset_session_admission(window, cx);
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        self.save_composer_draft(cx);
        self.save_scroll_position();
        if let Some(open) = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
        {
            open.selection
                .read(cx)
                .document
                .clone()
                .deactivate(window, cx);
        }
        self.prompt_panel = None;
        self.prompt_hover_task = None;
        self.prompt_hover_suppressed = false;
        self.session_menu = None;
        self.session_rename = None;
        self.branching.switch_runtime();
        self.context_pins.switch_runtime();
        self.message_focus.clear();
        if self.session_management.switch_runtime() {
            gpui_kit::component::WindowExt::close_dialog(window, cx);
        }
        self.code_wrap_save_task = None;
        self.code_wrap_state = CodeWrapState::default();
        self.invalidate_blocking(None);
        self.blocking.selected = None;
        cx.set_global(CodeWrap::default());
        let previous = std::mem::replace(&mut self.runtime, runtime);
        self.invalidate_terminal_client(cx);
        self.reset_queue_runtime(&previous);
        self.starters.reset();
        if self.goals.dialog.take().is_some() || self.reviews.dialog.take().is_some() {
            gpui_kit::component::WindowExt::close_dialog(window, cx);
        }
        self.goals.switch_runtime();
        self.reviews.switch_runtime();
        self.abort_primed = None;
        self.sessions = SessionIndex::new();
        self.topology = Topology::default();
        self.reset_send_catalog();
        self.permission_modes.reset();
        self.reset_project_readouts();
        self.reset_workflow_runs();
        self.reset_mcp_status();
        self.open_sessions.clear();
        self.live_status.clear();
        self.session_activity = SessionActivity::new();
        self.subagents_present.set(None);
        self.subagents_reopened.set(false);
        self.selected = None;
        self.project_chooser = None;
        self.composer_key = self.new_session_draft_key();
        self.restore_composer_draft(window, cx);
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

    fn subscribe_control(&mut self, generation: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let mut stream = {
            let _runtime = connection.network.enter();
            ControlStream::subscribe(client)
        };
        connection.control_task = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(item) = stream.next().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        if this
                            .connection
                            .as_ref()
                            .is_none_or(|connection| connection.generation != generation)
                        {
                            return;
                        }
                        match item {
                            ControlItem::Event(ControlEvent::Connected) => {
                                this.load_topology(window, cx);
                                this.refresh_terminal_sessions(cx);
                                // Workflow run events are not replayed.
                                this.reload_workflow_runs(cx);
                            }
                            ControlItem::Event(ControlEvent::TerminalSessionsChanged) => {
                                this.refresh_terminal_sessions(cx)
                            }
                            ControlItem::Event(
                                ControlEvent::WorktreeChanged { .. } | ControlEvent::SessionCreated,
                            ) => this.refresh_topology_soon(window, cx),
                            ControlItem::Event(ControlEvent::ScheduledTaskRan { project_id }) => {
                                // The page's read asks the workspace for its
                                // connection, so it starts after this update.
                                if let Some(page) = this.scheduled_tasks.as_ref() {
                                    let page = page.downgrade();
                                    cx.defer(move |cx| {
                                        let _ = page
                                            .update(cx, |page, cx| page.task_ran(&project_id, cx));
                                    });
                                }
                            }
                            ControlItem::Event(ControlEvent::ProjectContextChanged {
                                project_id,
                            }) => this.project_context_changed(&project_id, cx),
                            ControlItem::Event(ControlEvent::WorkflowRun(run)) => {
                                this.apply_workflow_run(*run, cx)
                            }
                            ControlItem::Event(ControlEvent::WorkflowRunRemoved {
                                parent_session_id,
                                run_id,
                            }) => this.remove_workflow_run(&parent_session_id, &run_id, cx),
                            ControlItem::Disconnected(error) => {
                                tracing::warn!(%error, "the topology control stream dropped")
                            }
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    fn refresh_topology_soon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.session_admission.topology_refresh =
            Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let _ = this.update_in(cx, |this, window, cx| this.load_topology(window, cx));
            }));
    }

    pub(crate) fn apply_stream_item(
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
            StreamItem::Connected {
                resumed,
                replay_through,
            } => {
                tracing::debug!(resumed, "the event stream is live");
                let stale = std::mem::take(&mut connection.stale);
                connection.state = ConnectionState::Connected;
                if stale {
                    // `left-for-user` is never replayed: the UI broadcaster
                    // bypasses the hub's replay buffer, so a gap may have
                    // lost the report even when the replay resumed. The
                    // window closes once the policy read below settled and
                    // the replay's last event was applied.
                    self.permission_modes.begin_replay(replay_through);
                }
                if !connection.loaded {
                    connection.loaded = true;
                    self.reconcile(window, cx);
                } else if stale && !resumed {
                    // Events were missed and not replayed: reread what they changed.
                    self.reconcile(window, cx);
                } else {
                    // Topology controls have no replay, independently of OpenCode's cursor.
                    self.load_topology(window, cx);
                }
                // Routing and permission modes are server reads outside the
                // OpenCode event replay, and their broadcasts are not
                // replayed. Refresh on stream connection even when that
                // replay resumed.
                self.load_auto_routing(cx);
                self.load_permission_policy(cx);
                // Queue broadcasts are not replayed either.
                self.load_message_queue(cx);
                // Nor are auto-review broadcasts.
                self.load_auto_reviews(cx);
                // Nor are warming and folder broadcasts.
                self.load_session_warming(cx);
                self.sync_session_folders(cx);
            }
            StreamItem::Disconnected(error) => {
                tracing::warn!(%error, "the event stream dropped");
                let failure = ConnectionFailure::from_error(&error);
                if matches!(
                    failure,
                    ConnectionFailure::AuthenticationRequired
                        | ConnectionFailure::AuthenticationRejected
                        | ConnectionFailure::ClientAuthenticationRequired
                        | ConnectionFailure::AuthenticationRateLimited { .. }
                ) {
                    connection.restart();
                    connection.retry_task = None;
                    connection.state = ConnectionState::Failed(failure);
                    self.reset_session_admission(window, cx);
                    self.show_authentication(failure, None, window, cx);
                    self.invalidate_terminal_client(cx);
                    cx.notify();
                    return;
                }
                connection.stale = true;
                connection.state = match failure {
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
            StreamItem::Events { events, event_id } => {
                self.apply_server_events(events, window, cx);
                if let Some(id) = event_id {
                    self.permission_modes.event_applied(&id);
                }
            }
        }
        cx.notify();
    }

    /// Rereads everything live state may have missed: sessions, topology,
    /// running state and every visited transcript.
    pub(crate) fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A gap may have lost the report that would reveal a held request.
        self.permission_modes.holds.release_all();
        // It may have lost status reports too: the snapshot read before it
        // certifies no session idle until this reread's own one applies.
        self.session_activity.withdraw_snapshot();
        self.invalidate_blocking(None);
        self.refresh_blocking_requests(window, cx);
        tracing::info!(
            open_sessions = self.open_sessions.len(),
            "reading sessions, topology and visited transcripts"
        );
        self.load_sessions(window, cx);
        self.load_topology(window, cx);
        let open: Vec<SessionId> = self.open_sessions.keys().cloned().collect();
        for id in open {
            if self.session_admitted(&id) {
                self.load_history(&id, cx);
            }
        }
    }

    pub(crate) fn load_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let generation = connection.generation;
        let ticket = self.sessions.begin_load();
        let runtime = self.runtime.clone();
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
            .run(async move { statuses_client.host_session_status().await });
        connection.sessions_task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut sessions: Vec<SessionRecord> = Vec::new();
            let mut first = true;
            while let Some(page) = receiver.recv().await {
                match page {
                    Ok(page) => {
                        if std::mem::take(&mut first) {
                            let early = page.clone();
                            let _ = this.update_in(cx, |this, window, cx| {
                                if this.runtime != runtime
                                    || this.connection.as_ref().is_none_or(|connection| {
                                        connection.generation != generation
                                    })
                                {
                                    return;
                                }
                                let update = this.sessions.merge_page(ticket, early);
                                this.after_index_update(update, window, cx);
                            });
                        }
                        sessions.extend(page);
                    }
                    Err(error) => {
                        let _ = this.update(cx, |this, cx| {
                            if this.runtime != runtime
                                || this
                                    .connection
                                    .as_ref()
                                    .is_none_or(|connection| connection.generation != generation)
                            {
                                return;
                            }
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
                if this.runtime != runtime
                    || this
                        .connection
                        .as_ref()
                        .is_none_or(|connection| connection.generation != generation)
                {
                    return;
                }
                // A failed status read is not an empty snapshot: nothing it
                // would have listed is known, so the earlier one stops
                // certifying sessions idle.
                if statuses.is_none() {
                    this.session_activity.withdraw_snapshot();
                }
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
                    if let Some(pending) = statuses.pending {
                        this.blocking.seeds.insert(this.runtime.clone(), pending);
                    }
                    this.apply_status_snapshot(statuses.statuses, statuses.turn_elapsed);
                    this.refresh_blocking_requests(window, cx);
                }
                this.after_index_update(update, window, cx);
                this.rebuild_sidebar();
                this.open_awaited_notification(window, cx);
                cx.notify();
            });
        }));
    }

    /// Replaces running state with the host's snapshot. A failed read keeps
    /// what the live channel established. `turn_elapsed` is how long each
    /// running turn had run when the host answered; it becomes a start on
    /// this device's clock.
    pub(crate) fn apply_status_snapshot(
        &mut self,
        statuses: HashMap<SessionId, LiveStatus>,
        turn_elapsed: HashMap<SessionId, i64>,
    ) {
        let known: Vec<SessionId> = self.live_status.keys().cloned().collect();
        self.live_status = statuses
            .into_iter()
            .filter(|(_, status)| *status != LiveStatus::Idle)
            .collect();
        let now = now_ms();
        let running: HashSet<SessionId> = self.live_status.keys().cloned().collect();
        let host_starts: Vec<(SessionId, i64)> = turn_elapsed
            .into_iter()
            .map(|(id, elapsed)| (id, now.saturating_sub(elapsed)))
            .collect();
        self.session_activity.apply_snapshot(&running, &host_starts);
        let active: Vec<&SessionId> = self.live_status.keys().collect();
        self.ordering
            .reconcile_snapshot(active, known.iter(), now_ms());
    }

    /// Turns code line wrapping on or off and saves the choice as the web
    /// does. A failed save keeps the choice on screen.
    pub(crate) fn toggle_code_wrap(&mut self, cx: &mut Context<Self>) {
        let wrap = !cx.try_global::<CodeWrap>().copied().unwrap_or_default().0;
        self.set_code_wrap(wrap, cx);
    }

    /// Settings' Chat page choice; the same writer as the code card toggle.
    pub(crate) fn set_code_wrap(&mut self, wrap: bool, cx: &mut Context<Self>) {
        self.show_code_wrap(wrap, cx);
        self.code_wrap_state.change(wrap);
        self.save_code_wrap(cx);
    }

    /// Settings' Retry: sends the shown choice again.
    pub(crate) fn retry_code_wrap(&mut self, cx: &mut Context<Self>) {
        if self.code_wrap_state.failed() {
            let wrap = cx.try_global::<CodeWrap>().copied().unwrap_or_default().0;
            self.set_code_wrap(wrap, cx);
        }
    }

    pub(crate) fn code_wrap_save_failed(&self) -> bool {
        self.code_wrap_state.failed()
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
            ..SettingsChanges::default()
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
                    if latest {
                        this.code_wrap_state.settle(result.is_ok());
                        cx.notify();
                    }
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
                        ..SettingsChanges::default()
                    };
                    let task = this.connection.as_ref()?.network.run(async move {
                        client.save_settings(&changes).await
                    });
                    Some((save, task))
                }).ok().flatten();
                }
            }));
    }

    pub(crate) fn load_topology(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let effort_read = self.begin_effort_settings_read();
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let generation = connection.generation;
        let runtime = self.runtime.clone();
        self.session_admission.topology_ticket =
            self.session_admission.topology_ticket.wrapping_add(1);
        let ticket = self.session_admission.topology_ticket;
        let baseline = self.session_admission.topology_revision;
        if self.send_catalog.settings.is_none() {
            self.send_catalog.settings_load = crate::send_catalog::Load::Loading;
        }
        let wrap_revision = self.code_wrap_state.revision();
        let handle = connection.network.handle();
        let initial_paths: Vec<String> = self
            .topology
            .projects
            .iter()
            .map(|project| project.path.clone())
            .collect();
        connection.topology_task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut need_roots = true;
            let mut need_settings = true;
            let mut paths = initial_paths;
            for attempt in 0..4 {
                if attempt > 0 {
                    cx.background_executor()
                        .timer(Duration::from_secs(1 << (attempt - 1)))
                        .await;
                }
                let Ok(layout_read) = this.update_in(cx, |this, _, cx| {
                    need_settings.then(|| this.begin_work_status_settings_read(cx))
                }) else {
                    return;
                };
                let client = client.clone();
                let requested_paths = paths.clone();
                let task = crate::network::NetworkTask::spawn_on(&handle, async move {
                    let roots = if need_roots {
                        Some(client.chat_roots().await)
                    } else {
                        None
                    };
                    let settings = if need_settings {
                        Some(client.settings().await)
                    } else {
                        None
                    };
                    let paths: Vec<String> = settings
                        .as_ref()
                        .and_then(|result| result.as_ref().ok())
                        .map(|settings| {
                            settings
                                .projects
                                .iter()
                                .map(|project| project.path.clone())
                                .collect()
                        })
                        .unwrap_or(requested_paths);
                    let mut reads = Vec::new();
                    for batch in paths.chunks(200) {
                        match client.project_worktrees(batch).await {
                            Ok(batch) => reads.extend(batch),
                            Err(error) => {
                                tracing::warn!(%error, "a worktree batch could not be read");
                                reads.extend(batch.iter().map(|path| {
                                    openchamber_native_transport::client::WorktreeRead {
                                        project_path: path.clone(),
                                        worktrees: None,
                                    }
                                }));
                            }
                        }
                    }
                    (roots, settings, reads)
                });
                let Some((roots, settings, worktrees)) = task.await else {
                    return;
                };
                let pending = this
                    .update_in(cx, |this, window, cx| {
                        if !this
                            .connection
                            .as_ref()
                            .is_some_and(|connection| connection.generation == generation)
                            || this.runtime != runtime
                            || this.session_admission.topology_ticket != ticket
                        {
                            return None;
                        }
                        let mut failed_roots = false;
                        let mut failed_settings = false;
                        // Each successful component commits independently of failed siblings.
                        match roots {
                            Some(Ok(roots)) => {
                                this.topology.chat_roots = roots;
                                this.session_admission.roots_ready = true;
                            }
                            Some(Err(error)) => {
                                failed_roots = true;
                                tracing::warn!(%error, "the Chats roots could not be read");
                            }
                            None => {}
                        }
                        if let Some(settings) = &settings {
                            this.send_catalog
                                .settings_load
                                .settle(settings.as_ref().ok().map(|_| ()));
                        }
                        match settings {
                            Some(Ok(mut settings)) => {
                                if let Some(layout_read) = &layout_read {
                                    this.reconcile_work_status_settings(
                                        layout_read,
                                        &mut settings,
                                        cx,
                                    );
                                }
                                // A registry read started before admission owns no removal of
                                // that confirmed append. A later fresh read can remove it.
                                this.session_admission
                                    .reconcile_projects(baseline, &mut settings.projects);
                                if let Some(previous) = &this.send_catalog.settings {
                                    settings.sidebar_view =
                                        settings.sidebar_view.or(previous.sidebar_view);
                                    settings.show_chats =
                                        settings.show_chats.or(previous.show_chats);
                                    settings.show_pinned =
                                        settings.show_pinned.or(previous.show_pinned);
                                    settings.show_work = settings.show_work.or(previous.show_work);
                                    settings.keep_work_in_place =
                                        settings.keep_work_in_place.or(previous.keep_work_in_place);
                                    settings.work_enabled =
                                        settings.work_enabled.or(previous.work_enabled);
                                    settings.prompt_navigator_enabled = settings
                                        .prompt_navigator_enabled
                                        .or(previous.prompt_navigator_enabled);
                                    settings.allow_prompting_subagent_sessions = settings
                                        .allow_prompting_subagent_sessions
                                        .or(previous.allow_prompting_subagent_sessions);
                                }
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
                                    settings.code_block_line_wrap = Some(
                                        cx.try_global::<CodeWrap>().copied().unwrap_or_default().0,
                                    );
                                }
                                this.reconcile_effort_settings(&effort_read, &mut settings);
                                this.overlay_unsettled_settings(&mut settings, cx);
                                this.send_catalog.settings = Some(settings);
                                this.session_admission
                                    .confirmed_projects
                                    .retain(|(revision, _)| *revision > baseline);
                            }
                            Some(Err(error)) => {
                                failed_settings = true;
                                tracing::warn!(%error, "the settings could not be read");
                            }
                            None => {}
                        }
                        let mut failed_paths = Vec::new();
                        for read in worktrees {
                            let Some(worktrees) = read.worktrees else {
                                tracing::warn!("a project's worktrees could not be listed");
                                failed_paths.push(read.project_path);
                                continue;
                            };
                            if !this
                                .topology
                                .projects
                                .iter()
                                .any(|project| project.path == read.project_path)
                            {
                                continue;
                            }
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
                        this.rebuild_sidebar();
                        this.reconcile_session_admission(window, cx);
                        cx.notify();
                        Some((failed_roots, failed_settings, failed_paths))
                    })
                    .ok()
                    .flatten();
                let Some((roots, settings, failed_paths)) = pending else {
                    return;
                };
                need_roots = roots;
                need_settings = settings;
                paths = failed_paths;
                if !need_roots && !need_settings && paths.is_empty() {
                    return;
                }
            }
        }));
    }

    /// Reads a visited session's newest messages and merges them.
    pub(crate) fn load_history(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        if !self.session_admitted(id) {
            return;
        }
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
        open.earlier = EarlierHistory::Idle;
        open.history_generation = open.history_generation.wrapping_add(1);
        let read = HistoryRead {
            session: id.clone(),
            runtime: self.runtime.clone(),
            connection_generation: connection.generation,
            generation: open.history_generation,
            since: open.transcript.revision(),
            requested: Instant::now(),
        };
        let request_id = id.clone();
        let task = connection
            .network
            .run(async move { client.latest_messages(&request_id, HISTORY_PAGE_SIZE).await });
        open.history = History::Loading {
            _read: cx.spawn(async move |this, cx| {
                let outcome = task.await.unwrap_or(Err(TransportError::Cancelled));
                let _ = this.update(cx, |this, cx| {
                    this.finish_history(read, outcome, cx);
                });
            }),
        };
    }

    fn finish_history(
        &mut self,
        read: HistoryRead,
        outcome: Result<MessagePage, TransportError>,
        cx: &mut Context<Self>,
    ) {
        if self.runtime != read.runtime
            || !self
                .connection
                .as_ref()
                .is_some_and(|connection| connection.generation == read.connection_generation)
            || !self
                .open_sessions
                .get(&read.session)
                .is_some_and(|open| open.history_generation == read.generation)
        {
            return;
        }
        let id = &read.session;
        let mutation_key = self.mutation_key(id);
        let before = self.send_attempts.ids(&mutation_key);
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
                let change = self.send_attempts.merge_history(
                    &mutation_key,
                    &mut open.transcript,
                    page.messages,
                    read.since,
                    HistoryMerge::Latest,
                );
                open.older = page.older;
                open.history = History::Complete;
                apply_list_change(open, &change, cx);
                if was_empty {
                    restore_position(open, &position);
                    let count = open.transcript.len();
                    self.mark_on_paint(Marker::HistoryShown, Some(read.requested), Some(count));
                }
            }
            Err(error) => {
                tracing::warn!(%error, "a session's history could not be read");
                open.history = History::Failed(ConnectionFailure::from_error(&error));
            }
        }
        self.forget_resolved(&mutation_key, &before, cx);
        cx.notify();
    }

    /// Loads one older page without replacing the held transcript or moving
    /// its visible anchor. Failure preserves the cursor for an explicit retry.
    pub(crate) fn load_earlier_prompts(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let Some(open) = self.open_sessions.get_mut(&id) else {
            return;
        };
        if matches!(open.earlier, EarlierHistory::Loading { .. }) {
            return;
        }
        let Some(cursor) = open.older.clone() else {
            return;
        };
        let generation = connection.generation;
        let runtime = self.runtime.clone();
        let since = open.transcript.revision();
        let history_generation = open.history_generation;
        let request_id = id.clone();
        let request_cursor = cursor.clone();
        let task = connection.network.run(async move {
            let page = client
                .earlier_messages(&request_id, &request_cursor, HISTORY_PAGE_SIZE)
                .await?;
            if page.older.as_ref() == Some(&request_cursor) {
                return Err(TransportError::Malformed(
                    "the older message cursor did not advance".to_owned(),
                ));
            }
            Ok(page)
        });
        open.earlier = EarlierHistory::Loading {
            _read: cx.spawn(async move |this, cx| {
                let result = task.await.unwrap_or(Err(TransportError::Cancelled));
                let _ = this.update(cx, |this, cx| {
                    if this.runtime != runtime
                        || !this
                            .connection
                            .as_ref()
                            .is_some_and(|connection| connection.generation == generation)
                    {
                        return;
                    }
                    let mutation_key = this.mutation_key(&id);
                    let before = this.send_attempts.ids(&mutation_key);
                    let Some(open) = this.open_sessions.get_mut(&id) else {
                        return;
                    };
                    if open.history_generation != history_generation
                        || open.older.as_ref() != Some(&cursor)
                    {
                        return;
                    }
                    match result {
                        Ok(page) => {
                            let before = crate::prompt_rail::prompt_rows(&open.transcript).len();
                            let change = this.send_attempts.merge_history(
                                &mutation_key,
                                &mut open.transcript,
                                page.messages,
                                since,
                                HistoryMerge::Earlier,
                            );
                            open.older = page.older;
                            open.earlier = EarlierHistory::Idle;
                            apply_list_change(open, &change, cx);
                            let added = crate::prompt_rail::prompt_rows(&open.transcript)
                                .len()
                                .saturating_sub(before);
                            if this.selected.as_ref() == Some(&id) {
                                this.prompt_history_prepended(added);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "earlier prompts could not be read");
                            open.earlier =
                                EarlierHistory::Failed(ConnectionFailure::from_error(&error));
                        }
                    }
                    this.forget_resolved(&mutation_key, &before, cx);
                    cx.notify();
                });
            }),
        };
        cx.notify();
    }

    pub(crate) fn apply_server_events(
        &mut self,
        events: Vec<ServerEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_server_events_with_deletion_cleanup(
            events,
            SessionDeletionCleanup::CurrentSubtree,
            window,
            cx,
        );
    }

    /// DELETE completions already contain their captured subtree. Keep the live
    /// reducer's index revisions and side effects within those exact identities.
    pub(crate) fn apply_confirmed_session_deletions(
        &mut self,
        sessions: &[SessionId],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let events = sessions
            .iter()
            .map(|id| ServerEvent::SessionDeleted {
                session_id: id.clone(),
            })
            .collect();
        self.apply_server_events_with_deletion_cleanup(
            events,
            SessionDeletionCleanup::CapturedSession,
            window,
            cx,
        );
    }

    fn apply_server_events_with_deletion_cleanup(
        &mut self,
        events: Vec<ServerEvent>,
        deletion_cleanup: SessionDeletionCleanup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut changes = Vec::new();
        let mut update = IndexUpdate::default();
        let mut ordering = Vec::new();
        let mut reread: Vec<SessionId> = Vec::new();
        let mut git_changed: Vec<SessionId> = Vec::new();
        let mut reverted: Vec<SessionId> = Vec::new();
        for event in events {
            if let ServerEvent::SessionPatched { session_id, patch } = &event {
                let key = self.mutation_key(session_id);
                if let Some(read) = self.branching.fork_reads.get_mut(&key) {
                    read.patches.push(patch.clone());
                }
                // Archiving discards the session's status: an earlier
                // snapshot cannot certify it idle after a restore.
                if matches!(patch.archived, Some(Some(_))) {
                    self.session_activity.invalidate(session_id);
                }
            }
            if let Some(change) = event.session_change() {
                changes.push(change);
            }
            match event {
                ServerEvent::BlockingRequest { directory, event } => {
                    // In order: a request sees the sessions created before it.
                    let earlier = self.sessions.apply(changes.drain(..));
                    update.changed |= earlier.changed;
                    update.moves.extend(earlier.moves);
                    self.blocking_event(directory, event, window, cx)
                }
                ServerEvent::BlockingRequestInvalid { directory, .. } => {
                    self.invalidate_blocking(directory.as_deref());
                    self.refresh_blocking_requests(window, cx);
                }
                ServerEvent::LocationShutdown { directory } => {
                    self.invalidate_blocking(Some(&directory));
                    self.mcp_location_shutdown(&directory, window, cx);
                }
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
                    self.session_activity.observe_status(&session_id, status);
                    ordering.push(OrderingMutation::Observe { session_id, phase });
                }
                // Stamped on arrival: the event's own time is the server's
                // clock, not this device's.
                ServerEvent::SessionExecution { session_id, edge } => match edge {
                    ExecutionEdge::Started => {
                        self.session_activity.turn_started(&session_id, now_ms())
                    }
                    ExecutionEdge::Failed => self.session_activity.turn_failed(&session_id),
                },
                ServerEvent::SessionDeleted { session_id } => {
                    self.branching
                        .fork_reads
                        .remove(&self.mutation_key(&session_id));
                    self.forget_session_queue(&session_id);
                    self.remove_session_requests(&session_id);
                    match deletion_cleanup {
                        SessionDeletionCleanup::CurrentSubtree => {
                            self.forget_deleted_session(&session_id, window, cx)
                        }
                        SessionDeletionCleanup::CapturedSession => self.forget_deleted_sessions(
                            &self.runtime.clone(),
                            std::slice::from_ref(&session_id),
                            window,
                            cx,
                        ),
                    }
                    self.live_status.remove(&session_id);
                    self.session_activity.forget(&session_id);
                    ordering.push(OrderingMutation::Remove { session_id });
                }
                ServerEvent::SessionForked {
                    session_id,
                    parent_id,
                } => self.on_session_forked(session_id, &parent_id, window, cx),
                // Before the batch's marker patch, which the trim reads.
                ServerEvent::RevertCommitted { session_id, to } => {
                    self.on_revert_committed(&session_id, &to, cx)
                }
                ServerEvent::SessionPatched { session_id, patch } if patch.revert.is_some() => {
                    reverted.push(session_id);
                }
                ServerEvent::CatalogUpdated { kind, directory } => {
                    self.refresh_send_catalog(kind, directory.as_deref(), cx);
                    if matches!(
                        kind,
                        openchamber_native_transport::event::CatalogKind::McpStatus
                    ) {
                        self.mcp_status_changed(directory.as_deref(), cx);
                    }
                }
                ServerEvent::PermissionPolicy(snapshot) => self.apply_permission_policy(snapshot),
                ServerEvent::PermissionLeftForUser {
                    session_id,
                    permission_id,
                    ..
                } => self.permission_left_for_user(session_id, permission_id),
                ServerEvent::RoutingUpdated => self.load_auto_routing(cx),
                ServerEvent::SessionWarming(snapshot) => self.apply_session_warming(snapshot),
                ServerEvent::SessionFoldersChanged { updated_at } => {
                    self.session_folders_changed(updated_at, cx)
                }
                ServerEvent::MessageQueueUpdated { update, space } => {
                    self.apply_queue_broadcast(update, space)
                }
                ServerEvent::AutoReviewUpdated(update) => self.apply_auto_review_update(update),
                ServerEvent::Notification(notification) => {
                    self.deliver_notification(notification, window, cx)
                }
                ServerEvent::Transcript { session_id, event } => {
                    if self.finishes_git_changing_call(&session_id, &event)
                        && !git_changed.contains(&session_id)
                    {
                        git_changed.push(session_id.clone());
                    }
                    if self.apply_transcript_event(&session_id, event, cx)
                        && !reread.contains(&session_id)
                    {
                        reread.push(session_id);
                    }
                }
                _ => {}
            }
        }
        let rest = self.sessions.apply(changes);
        update.changed |= rest.changed;
        update.moves.extend(rest.moves);
        let reordered = !ordering.is_empty() && self.ordering.apply(&ordering, now_ms());
        if reordered && !update.changed {
            self.rebuild_sidebar();
        }
        self.after_index_update(update, window, cx);
        self.open_btw_fork(cx);
        // The transcript's cut moved.
        for id in reverted {
            self.remeasure_transcript(&id);
        }
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
    fn apply_transcript_event(
        &mut self,
        id: &SessionId,
        event: TranscriptEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let mutation_key = matches!(
            &event,
            TranscriptEvent::MessageUpdated(info)
                if info.role == openchamber_native_core::transcript::Role::User
        )
        .then(|| self.mutation_key(id));
        let before = mutation_key
            .as_ref()
            .map(|key| self.send_attempts.ids(key))
            .unwrap_or_default();
        let Some(open) = self.open_sessions.get_mut(id) else {
            if let (Some(key), TranscriptEvent::MessageUpdated(info)) = (&mutation_key, &event)
                && self.send_attempts.observe(key, &info.id)
            {
                self.forget_resolved(key, &before, cx);
                self.queue_persistence(cx);
            }
            return false;
        };
        let change = match &mutation_key {
            Some(key) => self
                .send_attempts
                .apply_event(key, &mut open.transcript, event),
            None => open.transcript.apply(event),
        };
        let needs_read = if change == TranscriptChange::NeedsReconcile {
            // A read in flight covers what the event referred to.
            !matches!(open.history, History::Loading { .. })
        } else {
            apply_list_change(open, &change, cx);
            false
        };
        if let Some(key) = mutation_key {
            self.forget_resolved(&key, &before, cx);
        }
        needs_read
    }

    /// Rebuilds the sidebar after an index change and keeps the selection on
    /// a session that is still listed.
    pub(crate) fn after_index_update(
        &mut self,
        update: IndexUpdate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reconcile_session_admission(window, cx);
        if !update.moves.is_empty() {
            self.invalidate_blocking(None);
            self.refresh_blocking_requests(window, cx);
            // A move can land in a worktree this client has not discovered.
            self.load_topology(window, cx);
            self.observe_session_moves(&update.moves, window, cx);
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
        if selection_gone && self.archive_open {
            // Only the session closes; the archive page keeps the focus.
            self.close_selected_session(window, cx);
        } else if selection_gone {
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
pub(crate) fn apply_list_change(
    open: &OpenSession,
    change: &TranscriptChange,
    cx: &mut gpui_kit::App,
) {
    open.selection.update(cx, |selection, cx| {
        selection.sync(&open.transcript, change, cx)
    });
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

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use openchamber_native_core::ids::SessionId;
    use openchamber_native_core::transcript::TranscriptEvent;
    use openchamber_native_transport::prompt::PromptAdmission;

    #[gpui_kit::test]
    fn authoritative_admission_releases_an_unvisited_session(cx: &mut TestAppContext) {
        let (_, workspace) = crate::workspace_tests::open(cx);
        cx.update(|cx| {
            workspace.update(cx, |workspace, cx| {
                let session = SessionId::from("ses_unvisited_recovery");
                let key = workspace.mutation_key(&session);
                let attempt = crate::send_attempt::tests::attempt(&key);
                let attempt_id = attempt.prompt.message_id.clone();
                let info = attempt.optimistic.info.clone();
                assert!(!workspace.open_sessions.contains_key(&session));
                workspace.send_attempts.begin(key.clone(), attempt);
                workspace
                    .send_attempts
                    .finish(&key, &attempt_id, Err(PromptAdmission::Unknown));
                workspace.apply_transcript_event(
                    &session,
                    TranscriptEvent::MessageUpdated(info),
                    cx,
                );
                assert!(workspace.send_attempts.can_send(&key));
                assert!(workspace.send_attempts.attempt(&key, &attempt_id).is_none());
                assert!(
                    !workspace.open_sessions.contains_key(&session),
                    "admission must not open an unseen transcript"
                );
            })
        });
    }
}
