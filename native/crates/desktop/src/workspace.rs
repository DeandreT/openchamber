//! The workspace: sessions, topology, selection, drafts and transcripts from
//! either deterministic synthetic data or one configured OpenChamber server.
//!
//! This entity owns application lifetime after `main` opens the window.
//! Dropping it drops every playback task and the server connection, which
//! cancels streams, requests and the event subscription.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::input::{InputEvent, TextareaState};
use gpui_kit::{
    App, AppContext, Context, Entity, FocusHandle, FollowMode, ListAlignment, ListOffset,
    ListState, SharedString, Subscription, Task, Window, px,
};
use openchamber_native_core::chat_projection;
use openchamber_native_core::chat_roots::ChatRoots;
use openchamber_native_core::draft::{Draft, DraftKey, DraftStore, DraftTarget};
use openchamber_native_core::grouping::{self, GroupingContext, WorktreeSortOrder};
use openchamber_native_core::ids::{DraftId, MessageId, PartId, RuntimeId, SessionId};
use openchamber_native_core::ordering::{ActivityPhase, OrderingMutation, SessionOrdering};
use openchamber_native_core::ownership::{
    ConfiguredProject, OwnershipInputs, ProjectWorktrees, SessionOwnershipIndex, Surface,
};
use openchamber_native_core::scroll_memory::{ScrollKey, ScrollMemory, ScrollPosition};
use openchamber_native_core::session::{LiveStatus, SessionRecord};
use openchamber_native_core::session_index::SessionIndex;
use openchamber_native_core::sidebar_rows::{
    self, SectionSpacing, SidebarInputs, SidebarState, SidebarViewMode,
};
use openchamber_native_core::synthetic::{
    self, ConversationShape, ResponseScript, SyntheticWorkspace, WorkloadSize,
};
use openchamber_native_core::transcript::{
    Message, MessageInfo, Part, Role, Transcript, TranscriptChange, TranscriptEvent,
};
use openchamber_native_core::turn_stats::TurnStats;

use crate::chrome::InitialSize;
use crate::composer_view::ComposerSlot;
use crate::connection::{Connection, ConnectionFailure};
use crate::diagnostics::{self, Marker};
use crate::locale;
use crate::project_section::ProjectReadouts;
use crate::provider_logos::ProviderLogos;
use crate::send_catalog::SendCatalog;
use crate::sidebar_item::SidebarItem;

/// Delay between synthetic response chunks.
const CHUNK_INTERVAL: Duration = Duration::from_millis(24);
/// Pixels rendered beyond the viewport in either direction.
const TRANSCRIPT_OVERDRAW: f32 = 800.0;
const SIDEBAR_OVERDRAW: f32 = 300.0;
/// The draft of the new session the startup view composes. No project is
/// chosen yet, so it is keyed on the filesystem root of its runtime.
const NEW_SESSION_DRAFT: &str = "new-session";
const NEW_SESSION_DIRECTORY: &str = "/";

/// Where the workspace's data comes from.
#[derive(Debug)]
pub enum Source {
    /// Deterministic synthetic data; no network or provider calls.
    Synthetic(WorkloadSize),
    /// One explicitly configured OpenChamber server, not yet connected.
    Server(Box<Connection>),
}

/// Projects, worktrees and Chats roots: what display ownership needs.
#[derive(Debug, Default)]
pub struct Topology {
    pub projects: Vec<ConfiguredProject>,
    pub worktrees: Vec<ProjectWorktrees>,
    /// Absent until the server answered; Chats classification needs it.
    pub chat_roots: Option<ChatRoots>,
}

/// Where a visited session's history read stands.
pub enum History {
    /// The transcript holds an authoritative read (or synthetic data).
    Complete,
    /// A read is in flight; dropping the task cancels it.
    Loading { _read: Task<()> },
    /// The last read failed; the transcript keeps what it held.
    Failed(ConnectionFailure),
}

/// A visited session's transcript and its retained scroll state.
///
/// The list state stays alive while the client runs, so returning to a
/// session restores the exact message and pixel offset the reader left.
pub struct OpenSession {
    pub transcript: Transcript,
    pub list: ListState,
    pub history: History,
    /// The cursor of the next older page, when the server has one.
    pub older: Option<String>,
    /// The prompt the rail last showed as current.
    pub rail_current: Option<usize>,
    /// The statistics the Turn stats section last committed while idle.
    pub turn_stats: Cell<Option<TurnStats>>,
}

pub struct Workspace {
    pub(crate) topology: Topology,
    pub(crate) sessions: SessionIndex,
    pub(crate) runtime: RuntimeId,
    pub(crate) ordering: SessionOrdering,
    pub(crate) sidebar_state: SidebarState,
    pub(crate) sidebar_items: Vec<SidebarItem>,
    /// The gap below each row of `sidebar_items` that opens the next section.
    pub(crate) sidebar_spacing: Vec<Option<SectionSpacing>>,
    pub(crate) sidebar_list: ListState,
    pub(crate) sidebar_open: bool,
    pub(crate) work_status_open: bool,
    /// Whether the work status card's Context sources section is expanded.
    pub(crate) work_status_context_open: bool,
    /// Turn stats start expanded.
    pub(crate) work_status_telemetry_open: bool,
    /// A press in a title row that moves the window once the pointer moves.
    pub(crate) title_drag_armed: bool,
    pub(crate) selected: Option<SessionId>,
    pub(crate) open_sessions: HashMap<SessionId, OpenSession>,
    pub(crate) drafts: DraftStore,
    pub(crate) scroll_memory: ScrollMemory,
    pub(crate) composer: Entity<TextareaState>,
    pub(crate) composer_key: Option<DraftKey>,
    /// Whether the composer shows the short placeholder; unknown until the
    /// first frame measures it.
    pub(crate) composer_placeholder_compact: Option<bool>,
    playback: HashMap<SessionId, Task<()>>,
    /// Running state per session, from the live channel only.
    pub(crate) live_status: HashMap<SessionId, LiveStatus>,
    /// Present when the source is a server.
    pub(crate) connection: Option<Connection>,
    /// The composer's catalogs and defaults; empty for synthetic data.
    pub(crate) send_catalog: SendCatalog,
    pub(crate) provider_logos: ProviderLogos,
    pub(crate) project_readouts: ProjectReadouts,
    /// Set while the new window's content is held at its requested size.
    pub(crate) initial_size: Option<InitialSize>,
    /// The docked composer's slot, measured while painting.
    pub(crate) composer_slot: Rc<Cell<ComposerSlot>>,
    /// The providers whose logos the sidebar rows show, without repeats.
    pub(crate) sidebar_providers: Vec<SharedString>,
    prompt_sequence: usize,
    pub(crate) focus_handle: FocusHandle,
    pending_switch: Option<Instant>,
    /// Markers for the next painted frame.
    pending_markers: Vec<(Marker, Option<Instant>, Option<usize>)>,
    startup_recorded: bool,
    /// Whether a complete session list was painted yet.
    pub(crate) sessions_shown: bool,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(source: Source, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut sessions = SessionIndex::new();
        let (topology, runtime, connection) = match source {
            Source::Synthetic(size) => {
                let data = SyntheticWorkspace::new(size);
                let ticket = sessions.begin_load();
                let _ = sessions.complete_load(ticket, data.sessions);
                let topology = Topology {
                    projects: data.projects,
                    worktrees: data.worktrees,
                    chat_roots: Some(data.chat_roots),
                };
                (topology, RuntimeId::from("synthetic"), None)
            }
            Source::Server(connection) => {
                let runtime = RuntimeId::new(connection.config.endpoint.as_str().to_owned());
                (Topology::default(), runtime, Some(*connection))
            }
        };
        let placeholder = locale::strings(cx).get("chat.chatInput.placeholder.chat");
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 10)
                .submit_on_enter(true)
                .placeholder(placeholder)
        });
        let subscription = cx.subscribe_in(&composer, window, Self::on_composer_event);

        let mut workspace = Self {
            topology,
            sessions,
            runtime,
            ordering: SessionOrdering::new(),
            sidebar_state: SidebarState::default(),
            sidebar_items: Vec::new(),
            sidebar_spacing: Vec::new(),
            sidebar_list: ListState::new(0, ListAlignment::Top, px(SIDEBAR_OVERDRAW)),
            sidebar_open: true,
            work_status_open: true,
            work_status_context_open: false,
            work_status_telemetry_open: true,
            title_drag_armed: false,
            selected: None,
            open_sessions: HashMap::new(),
            drafts: DraftStore::new(),
            scroll_memory: ScrollMemory::new(),
            composer,
            composer_key: None,
            composer_placeholder_compact: None,
            playback: HashMap::new(),
            live_status: HashMap::new(),
            connection,
            send_catalog: SendCatalog::default(),
            provider_logos: ProviderLogos::default(),
            project_readouts: ProjectReadouts::default(),
            initial_size: None,
            composer_slot: Rc::default(),
            sidebar_providers: Vec::new(),
            prompt_sequence: 0,
            focus_handle: cx.focus_handle(),
            pending_switch: None,
            pending_markers: Vec::new(),
            startup_recorded: false,
            sessions_shown: false,
            _subscriptions: vec![subscription],
        };
        workspace.rebuild_sidebar();
        // The window opens on a new session, as the web client does.
        workspace.composer_key = workspace.new_session_draft_key();
        workspace.focus_composer(window, cx);
        if workspace.connection.is_some() {
            workspace.connect(window, cx);
        }
        workspace
    }

    /// Whether the workspace shows synthetic data.
    pub fn is_synthetic(&self) -> bool {
        self.connection.is_none()
    }

    /// The selected session's record.
    pub fn selected_session(&self) -> Option<&SessionRecord> {
        let id = self.selected.as_ref()?;
        self.session(id)
    }

    pub fn session(&self, id: &SessionId) -> Option<&SessionRecord> {
        self.sessions
            .get(id)
            .filter(|session| !session.is_archived())
    }

    /// Whether the session is producing output: synthetic playback, or a
    /// running turn the live channel reported.
    pub fn is_streaming(&self, id: &SessionId) -> bool {
        self.playback.contains_key(id)
            || matches!(
                self.live_status.get(id),
                Some(LiveStatus::Busy | LiveStatus::Retry)
            )
    }

    /// Rebuilds the sidebar rows from the core projection.
    ///
    /// The row at the top of the viewport keeps its position when rows are
    /// inserted or removed elsewhere.
    pub fn rebuild_sidebar(&mut self) {
        let started = Instant::now();
        let (items, spacing) = build_sidebar_items(
            &self.topology,
            self.sessions.active(),
            self.sessions.archived(),
            &self.sidebar_state,
            &mut self.ordering,
        );
        let elapsed = started.elapsed();
        let top = self.sidebar_list.logical_scroll_top();
        let anchor = self
            .sidebar_items
            .get(top.item_ix)
            .map(|item| item.key().to_owned());
        let count = items.len();
        let mut providers: Vec<SharedString> = Vec::new();
        for item in &items {
            if let SidebarItem::Session {
                provider_id: Some(provider_id),
                ..
            } = item
                && !providers.contains(provider_id)
            {
                providers.push(provider_id.clone());
            }
        }
        self.sidebar_providers = providers;
        self.sidebar_items = items;
        self.sidebar_spacing = spacing;
        self.sidebar_list.reset(count);
        let restored =
            anchor.and_then(|key| self.sidebar_items.iter().position(|item| item.key() == key));
        if let Some(item_ix) = restored {
            self.sidebar_list.scroll_to(ListOffset {
                item_ix,
                offset_in_item: top.offset_in_item,
            });
        }
        diagnostics::mark(Marker::SidebarRebuilt, Some(elapsed), Some(count));
    }

    pub(crate) fn new_session_draft_key(&self) -> Option<DraftKey> {
        DraftKey::new(
            self.runtime.clone(),
            NEW_SESSION_DIRECTORY,
            DraftTarget::NewSession(DraftId::from(NEW_SESSION_DRAFT)),
        )
    }

    /// Leaves the selected session for the new-session view, keeping both
    /// drafts and the session's reading position.
    pub fn show_new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_some() {
            self.save_composer_draft(cx);
            self.save_scroll_position();
            self.selected = None;
            self.composer_key = self.new_session_draft_key();
            self.restore_composer_draft(window, cx);
        }
        self.focus_composer(window, cx);
        cx.notify();
    }

    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    pub fn toggle_work_status(&mut self, cx: &mut Context<Self>) {
        self.work_status_open = !self.work_status_open;
        cx.notify();
    }

    /// Selects a session: saves the outgoing draft and position under their
    /// captured identities, then restores the incoming ones before paint.
    pub fn select_session(&mut self, id: SessionId, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&id) {
            self.focus_composer(window, cx);
            return;
        }
        let Some(session) = self.session(&id) else {
            return;
        };
        let next_key = session.directory.as_deref().and_then(|directory| {
            DraftKey::new(
                self.runtime.clone(),
                directory,
                DraftTarget::Session(id.clone()),
            )
        });
        let started = Instant::now();
        self.save_composer_draft(cx);
        self.save_scroll_position();
        self.selected = Some(id.clone());
        self.ensure_open(&id, cx);
        self.composer_key = next_key;
        self.restore_composer_draft(window, cx);
        self.focus_composer(window, cx);
        self.pending_switch = Some(started);
        cx.notify();
    }

    /// Moves the selection through session rows in display order.
    pub fn select_adjacent(&mut self, offset: isize, window: &mut Window, cx: &mut Context<Self>) {
        let sessions: Vec<&SessionId> = self
            .sidebar_items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::Session { session_id, .. } => Some(session_id),
                _ => None,
            })
            .collect();
        if sessions.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| sessions.iter().position(|id| *id == selected));
        let next = match current {
            Some(index) => index.saturating_add_signed(offset).min(sessions.len() - 1),
            None => 0,
        };
        let id = sessions[next].clone();
        if let Some(row) = self.sidebar_items.iter().position(
            |item| matches!(item, SidebarItem::Session { session_id, .. } if *session_id == id),
        ) {
            self.sidebar_list.scroll_to_reveal_item(row);
        }
        self.select_session(id, window, cx);
    }

    pub fn toggle_expanded(&mut self, expansion_key: &str, cx: &mut Context<Self>) {
        self.sidebar_state.toggle_expanded(expansion_key);
        self.rebuild_sidebar();
        cx.notify();
    }

    pub fn toggle_collapsed(&mut self, item: &SidebarItem, cx: &mut Context<Self>) {
        let state = &mut self.sidebar_state;
        match item {
            SidebarItem::Zone { activity, .. } => {
                if !state.collapsed_activities.remove(activity) {
                    state.collapsed_activities.insert(*activity);
                }
            }
            SidebarItem::Project { project_id, .. } => {
                if !state.collapsed_projects.remove(project_id) {
                    state.collapsed_projects.insert(project_id.clone());
                }
            }
            SidebarItem::Group { group_key, .. } => {
                if !state.collapsed_groups.remove(group_key) {
                    state.collapsed_groups.insert(group_key.clone());
                }
            }
            _ => return,
        }
        self.rebuild_sidebar();
        cx.notify();
    }

    pub fn reveal(&mut self, item: &SidebarItem, cx: &mut Context<Self>) {
        let SidebarItem::Show {
            control,
            container_key,
            current_count,
            increment,
            ..
        } = item
        else {
            return;
        };
        self.sidebar_state
            .reveal(container_key, *control, *current_count, *increment);
        self.rebuild_sidebar();
        cx.notify();
    }

    fn ensure_open(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        if self.open_sessions.contains_key(id) {
            return;
        }
        let transcript = if self.is_synthetic() {
            Transcript::from_history(synthetic::conversation(
                id,
                ConversationShape::for_session(id),
            ))
        } else {
            Transcript::new()
        };
        let list = ListState::new(
            transcript.len(),
            ListAlignment::Top,
            px(TRANSCRIPT_OVERDRAW),
        );
        list.set_follow_mode(FollowMode::Tail);
        // A scroll that moves the current prompt redraws the rail. The list
        // is still borrowed while it reports the scroll, so the rail reads it
        // once the scroll is applied.
        let workspace = cx.entity().downgrade();
        let session = id.clone();
        list.set_scroll_handler(move |_, _, cx| {
            let workspace = workspace.clone();
            let session = session.clone();
            cx.defer(move |cx| {
                let _ = workspace.update(cx, |this, cx| {
                    if this.refresh_prompt_rail(&session) {
                        cx.notify();
                    }
                });
            });
        });
        let key = ScrollKey {
            runtime: self.runtime.clone(),
            session: id.clone(),
        };
        match self.scroll_memory.position(&key) {
            ScrollPosition::FollowingTail => list.scroll_to_end(),
            ScrollPosition::Anchored {
                message_id,
                offset_in_message,
            } => {
                if let Some(item_ix) = transcript.row_of(message_id) {
                    list.scroll_to(ListOffset {
                        item_ix,
                        offset_in_item: px(*offset_in_message),
                    });
                }
            }
        }
        self.open_sessions.insert(
            id.clone(),
            OpenSession {
                transcript,
                list,
                history: History::Complete,
                older: None,
                rail_current: None,
                turn_stats: Cell::new(None),
            },
        );
        if !self.is_synthetic() {
            self.load_history(id, cx);
        }
    }

    fn save_scroll_position(&mut self) {
        let Some(id) = &self.selected else {
            return;
        };
        let Some(open) = self.open_sessions.get(id) else {
            return;
        };
        let position = if open.list.is_following_tail() {
            ScrollPosition::FollowingTail
        } else {
            let top = open.list.logical_scroll_top();
            match open.transcript.message(top.item_ix) {
                Some(message) => ScrollPosition::Anchored {
                    message_id: message.info.id.clone(),
                    offset_in_message: f32::from(top.offset_in_item),
                },
                None => ScrollPosition::FollowingTail,
            }
        };
        self.scroll_memory.save(
            ScrollKey {
                runtime: self.runtime.clone(),
                session: id.clone(),
            },
            position,
        );
    }

    fn save_composer_draft(&mut self, cx: &App) {
        let Some(key) = self.composer_key.clone() else {
            return;
        };
        let composer = self.composer.read(cx);
        let draft = Draft {
            text: composer.value().to_string(),
            selection: composer.selected_range(),
        };
        self.drafts.save(key, draft);
    }

    fn restore_composer_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self
            .composer_key
            .as_ref()
            .and_then(|key| self.drafts.get(key))
            .cloned()
            .unwrap_or_default();
        self.composer.update(cx, |composer, cx| {
            composer.set_value(draft.text.clone(), window, cx);
            composer.set_selected_range(draft.selection.clone(), cx);
        });
    }

    pub fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |composer, cx| composer.focus(window, cx));
    }

    fn on_composer_event(
        &mut self,
        _composer: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { shift: false, .. } = event {
            self.submit(window, cx);
        }
    }

    /// Sends the composer text to the selected session's synthetic stream.
    ///
    /// A server connection is read-only until prompt submission has its
    /// error and reconciliation paths; the draft stays in the composer.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_synthetic() {
            return;
        }
        let Some(session_id) = self.selected.clone() else {
            return;
        };
        if self.is_streaming(&session_id) {
            return;
        }
        let text = self.composer.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        if let Some(key) = &self.composer_key {
            let _ = self.drafts.take(key);
        }
        self.composer
            .update(cx, |composer, cx| composer.set_value("", window, cx));

        self.prompt_sequence += 1;
        let sequence = self.prompt_sequence;
        let now = synthetic_clock(sequence);
        let user_id = MessageId::new(format!("{session_id}-local-{sequence:04}-user"));
        let assistant_id = MessageId::new(format!("{session_id}-local-{sequence:04}-assistant"));
        let part_id = PartId::new(format!("{assistant_id}-p0"));
        let Some(open) = self.open_sessions.get_mut(&session_id) else {
            return;
        };
        for message in [
            Message {
                info: MessageInfo {
                    id: user_id.clone(),
                    role: Role::User,
                    created: now,
                    completed: None,
                    notice: None,
                    assistant: None,
                    compaction: None,
                },
                parts: vec![Part::text(PartId::new(format!("{user_id}-p0")), text)],
            },
            Message {
                info: MessageInfo {
                    id: assistant_id.clone(),
                    role: Role::Assistant,
                    created: now,
                    completed: None,
                    notice: None,
                    assistant: None,
                    compaction: None,
                },
                parts: vec![Part::text(part_id.clone(), String::new())],
            },
        ] {
            if let TranscriptChange::Inserted { rows } = open.transcript.push(message) {
                open.list.splice(rows.start..rows.start, rows.len());
            }
        }
        open.list.scroll_to_end();
        self.observe_phase(&session_id, ActivityPhase::Active, now);

        let script = ResponseScript::new(sequence);
        let playback_key = session_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let started = Instant::now();
            for chunk in script.chunks() {
                cx.background_executor().timer(CHUNK_INTERVAL).await;
                let applied = this.update(cx, |this, cx| {
                    this.apply_delta(&session_id, &assistant_id, &part_id, chunk, cx);
                });
                if applied.is_err() {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.finish_playback(&session_id, &assistant_id, now, cx);
            });
            diagnostics::mark(
                Marker::PlaybackCompleted,
                Some(started.elapsed()),
                Some(script.chunks().len()),
            );
        });
        self.playback.insert(playback_key, task);
        cx.notify();
    }

    /// Resumes tail following in the selected transcript.
    pub fn scroll_to_tail(&mut self, cx: &mut Context<Self>) {
        if let Some(open) = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
        {
            open.list.set_follow_mode(FollowMode::Tail);
            open.list.scroll_to_end();
        }
        cx.notify();
    }

    /// Stops the selected session's stream, keeping the text received so far.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        if self.playback.remove(&id).is_some() {
            self.observe_phase(
                &id,
                ActivityPhase::Settled,
                synthetic_clock(self.prompt_sequence),
            );
            cx.notify();
        }
    }

    fn apply_delta(
        &mut self,
        session_id: &SessionId,
        message_id: &MessageId,
        part_id: &PartId,
        chunk: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open_sessions.get_mut(session_id) else {
            return;
        };
        let change = open.transcript.apply(TranscriptEvent::PartDelta {
            message_id: message_id.clone(),
            part_id: part_id.clone(),
            delta: chunk.to_owned(),
        });
        if let TranscriptChange::Updated { row } = change {
            open.list.remeasure_items(row..row + 1);
            cx.notify();
        }
    }

    fn finish_playback(
        &mut self,
        session_id: &SessionId,
        message_id: &MessageId,
        started: i64,
        cx: &mut Context<Self>,
    ) {
        self.playback.remove(session_id);
        if let Some(open) = self.open_sessions.get_mut(session_id)
            && let Some(row) = open.transcript.row_of(message_id)
            && let Some(message) = open.transcript.message(row)
        {
            let mut info = message.info.clone();
            info.completed = Some(started + 1);
            let _ = open.transcript.apply(TranscriptEvent::MessageUpdated(info));
        }
        self.observe_phase(session_id, ActivityPhase::Settled, started + 1);
        cx.notify();
    }

    pub(crate) fn observe_phase(&mut self, session_id: &SessionId, phase: ActivityPhase, now: i64) {
        let changed = self.ordering.apply(
            &[OrderingMutation::Observe {
                session_id: session_id.clone(),
                phase,
            }],
            now,
        );
        if changed {
            self.rebuild_sidebar();
        }
    }

    /// Pending timing markers, emitted when the frame being built paints.
    ///
    /// Paint time ends the measured interval: the transcript the reader asked
    /// for is in the frame. Compositor presentation comes after and is not
    /// included, so an occluded window still reports its own work.
    pub(crate) fn take_frame_markers(&mut self) -> Vec<(Marker, Option<Instant>, Option<usize>)> {
        let mut markers = std::mem::take(&mut self.pending_markers);
        if !self.startup_recorded {
            self.startup_recorded = true;
            markers.push((Marker::ContentPainted, None, None));
            markers.push((Marker::ComposerUsable, None, None));
        }
        if let Some(started) = self.pending_switch.take() {
            markers.push((Marker::SwitchCompleted, Some(started), None));
        }
        markers
    }

    /// Emits `marker` when the next frame paints.
    pub(crate) fn mark_on_paint(
        &mut self,
        marker: Marker,
        started: Option<Instant>,
        count: Option<usize>,
    ) {
        self.pending_markers.push((marker, started, count));
    }
}

/// Synthetic timestamps advance with each prompt, independent of the wall clock.
fn synthetic_clock(sequence: usize) -> i64 {
    1_790_000_000_000 + sequence as i64 * 60_000
}

/// Runs the core projection and snapshots its rows for rendering, with the
/// gap below each row.
fn build_sidebar_items(
    topology: &Topology,
    active: &[SessionRecord],
    archived: &[SessionRecord],
    state: &SidebarState,
    ordering: &mut SessionOrdering,
) -> (Vec<SidebarItem>, Vec<Option<SectionSpacing>>) {
    let ownership = SessionOwnershipIndex::build(&OwnershipInputs {
        sessions: active,
        archived_sessions: archived,
        projects: &topology.projects,
        worktrees: &topology.worktrees,
        surface: Surface::Desktop,
        ..OwnershipInputs::default()
    });
    let chats = chat_projection::project(
        active,
        &ownership,
        topology.chat_roots.as_ref(),
        Surface::Desktop,
    );
    let branches = HashMap::new();
    let context = GroupingContext {
        ownership: &ownership,
        spaces: &[],
        surface: Surface::Desktop,
        worktree_sort: WorktreeSortOrder::Manual,
        branches: &branches,
    };
    let mut sections = Vec::with_capacity(topology.projects.len());
    for project in &topology.projects {
        let mut sessions: Vec<&SessionRecord> = chats
            .project_sessions_by_project
            .get(&project.id)
            .cloned()
            .unwrap_or_default();
        sessions.sort_by(|left, right| {
            ordering.compare(
                left,
                right,
                left.overlay.pinned_at.is_some(),
                right.overlay.pinned_at.is_some(),
            )
        });
        let worktrees = topology
            .worktrees
            .iter()
            .find(|entry| entry.project_path == project.path)
            .map(|entry| entry.worktrees.as_slice())
            .unwrap_or_default();
        sections.push(grouping::build_project_section(
            project, &sessions, worktrees, &context, ordering,
        ));
    }
    let chat_group = grouping::build_chat_group(&chats.chat_sessions, topology.chat_roots.as_ref());
    // Pinned is hidden by default (`showPinnedSection: false`), so pinned
    // sessions stay in their projects.
    let (_pinned, work) = sidebar_rows::zone_items(&sections, chat_group.as_ref(), ordering);
    let timeline = sidebar_rows::timeline_items(&sections, ordering);
    let group_status = HashMap::new();
    let model = sidebar_rows::build(
        &SidebarInputs {
            view_mode: SidebarViewMode::Timeline,
            detailed_zone_rows: false,
            chat_group: chat_group.as_ref(),
            pinned: &[],
            work: &work,
            timeline: &timeline,
            sections: &sections,
            state,
            group_status: &group_status,
            show_only_main_workspace: false,
            active_project_id: None,
        },
        ordering,
    );
    let project_paths: HashMap<_, _> = topology
        .projects
        .iter()
        .map(|project| (&project.id, project.path.as_str()))
        .collect();
    let items = model
        .rows
        .iter()
        .map(|row| SidebarItem::from_row(row, &project_paths))
        .collect();
    let spacing = model
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| sidebar_rows::section_spacing_after(row, model.rows.get(index + 1)))
        .collect();
    (items, spacing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_representative_sidebar_projects_without_panicking() {
        let data = SyntheticWorkspace::new(WorkloadSize::Representative);
        let (archived, active): (Vec<SessionRecord>, Vec<SessionRecord>) = data
            .sessions
            .iter()
            .cloned()
            .partition(SessionRecord::is_archived);
        let topology = Topology {
            projects: data.projects,
            worktrees: data.worktrees,
            chat_roots: Some(data.chat_roots),
        };
        let (items, spacing) = build_sidebar_items(
            &topology,
            &active,
            &archived,
            &SidebarState::default(),
            &mut SessionOrdering::new(),
        );
        assert_eq!(items.len(), spacing.len(), "one gap entry per row");
        assert!(
            items
                .iter()
                .any(|item| matches!(item, SidebarItem::Zone { .. }))
        );
        assert!(
            items
                .iter()
                .any(|item| matches!(item, SidebarItem::Project { .. }))
        );
        let mut keys: Vec<&str> = items.iter().map(SidebarItem::key).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "row keys must be unique");
    }
}
