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
use openchamber_native_core::sidebar_collection::SidebarCollection;
use openchamber_native_core::sidebar_rows::{self, SectionSpacing, SidebarInputs, SidebarState};
use openchamber_native_core::synthetic::{
    self, ConversationShape, ResponseScript, SyntheticWorkspace, WorkloadSize,
};
use openchamber_native_core::transcript::{
    Message, MessageInfo, Part, Role, Transcript, TranscriptChange, TranscriptEvent,
};
use openchamber_native_core::turn_stats::TurnStats;
use openchamber_native_transport::client::ServerSettings;

use crate::chrome::InitialSize;
use crate::code_card::CodeWrap;
use crate::code_wrap_state::CodeWrapState;
use crate::composer_view::ComposerSlot;
use crate::connection::{Connection, ConnectionFailure};
use crate::diagnostics::{self, Marker};
use crate::locale;
use crate::project_section::ProjectReadouts;
use crate::prompt_rail::PromptPanel;
use crate::provider_logos::ProviderLogos;
use crate::send_catalog::SendCatalog;
use crate::sidebar_item::SidebarItem;

/// Delay between synthetic response chunks.
const CHUNK_INTERVAL: Duration = Duration::from_millis(24);
/// Pixels rendered beyond the viewport in either direction.
const TRANSCRIPT_OVERDRAW: f32 = 800.0;
const SIDEBAR_OVERDRAW: f32 = 300.0;
/// Unassigned and directory-bound drafts keep different IDs even when the
/// chosen project is the filesystem root.
pub(crate) const PROJECT_DRAFT_ID: &str = "new-session";
const UNASSIGNED_DRAFT_ID: &str = "unassigned-new-session";
const NEW_SESSION_DIRECTORY: &str = "/";

/// Where the workspace's data comes from.
#[derive(Debug)]
pub(crate) enum Source {
    /// Deterministic synthetic data; no network or provider calls.
    Synthetic(WorkloadSize),
    /// One explicitly configured OpenChamber server, not yet connected.
    Server(Box<Connection>),
    /// A saved-host read failed. Keep the file intact and ask for recovery.
    HostRecovery {
        store: crate::host_profiles::ProfileStore,
        error: crate::host_profiles::ProfilesError,
    },
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

pub enum EarlierHistory {
    Idle,
    Loading { _read: Task<()> },
    Failed(ConnectionFailure),
}

/// A visited session's transcript and its retained scroll state.
///
/// The list state stays alive while the client runs, so returning to a
/// session restores the exact message and pixel offset the reader left.
pub struct OpenSession {
    pub transcript: Transcript,
    pub(crate) selection: Entity<crate::transcript_selection::TranscriptSelection>,
    pub list: ListState,
    pub history: History,
    pub(crate) history_generation: u64,
    /// The cursor of the next older page, when the server has one.
    pub older: Option<String>,
    pub earlier: EarlierHistory,
    /// The prompt the rail last showed as current.
    pub rail_current: Option<usize>,
    /// The statistics the Turn stats section last committed while idle.
    pub turn_stats: Cell<Option<TurnStats>>,
    /// Rows and blocks the reader opened, and whole-message reads.
    pub(crate) parts: crate::part_state::PartState,
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
    /// The sidebar's and the context panel's widths and their drag.
    pub(crate) pane_sizes: crate::pane_resize::PaneSizes,
    pub(crate) work_status_open: bool,
    pub(crate) work_status_overlay: bool,
    /// The terminal panel, kept while hidden so its terminals keep parsing.
    pub(crate) terminal_panel: Option<Entity<crate::terminal_view::TerminalPanel>>,
    pub(crate) terminal_open: bool,
    pub(crate) prompt_panel: Option<PromptPanel>,
    pub(crate) prompt_hover_task: Option<Task<()>>,
    pub(crate) prompt_hover_suppressed: bool,
    /// Whether the work status card's Context sources section is expanded.
    pub(crate) work_status_context_open: bool,
    /// Turn stats start expanded.
    pub(crate) work_status_telemetry_open: bool,
    /// A press in a title row that moves the window once the pointer moves.
    pub(crate) title_drag_armed: bool,
    pub(crate) selected: Option<SessionId>,
    pub(crate) open_sessions: HashMap<SessionId, OpenSession>,
    pub(crate) drafts: DraftStore,
    pub(crate) attachments: crate::attachment_catalog::AttachmentCatalog,
    pub(crate) attachment_preparations: Vec<crate::attachment_catalog::PreparationHold>,
    pub(crate) attachment_staging: crate::attachment_staging::AttachmentStaging,
    pub(crate) attachment_retirement: crate::attachment_retirement::AttachmentRetirement,
    pub(crate) attachment_storage: crate::attachment_storage::AttachmentStorage,
    pub(crate) attachment_storage_close: Option<Task<()>>,
    pub(crate) retired_attachment_blobs: Vec<crate::attachment_spool::BlobId>,
    pub(crate) scroll_memory: ScrollMemory,
    pub(crate) composer: Entity<TextareaState>,
    pub(crate) composer_documents: crate::composer_document::ComposerDocuments,
    pub(crate) attachment_input: crate::composer_attachment_input::ComposerAttachmentInput,
    pub(crate) autocomplete: crate::composer_autocomplete::ComposerAutocomplete,
    pub(crate) session_references: crate::composer_references::SessionReferences,
    pub(crate) composer_key: Option<DraftKey>,
    pub(crate) draft_previews: HashMap<DraftKey, SharedString>,
    pub(crate) draft_rows: HashMap<DraftKey, usize>,
    pub(crate) draft_projects: HashMap<RuntimeId, String>,
    pub(crate) project_chooser: Option<crate::project_chooser::ProjectChooser>,
    pub(crate) session_menu: Option<crate::session_menu::SessionMenu>,
    pub(crate) app_menu: crate::app_menu::AppMenuState,
    pub(crate) session_rename: Option<crate::session_menu::SessionRename>,
    pub(crate) session_management: crate::session_management::SessionManagement,
    pub(crate) branching: crate::session_branching::Branching,
    /// The archive page replaces the chat area while open.
    pub(crate) archive_open: bool,
    /// The issues and pull requests board replaces the chat area while open.
    pub(crate) source_board_open: bool,
    /// The board, kept while the runtime stays so it reopens where it was left.
    pub(crate) source_board: Option<Entity<crate::source_board::SourceBoard>>,
    /// The scheduled tasks page replaces the chat area while open.
    pub(crate) scheduled_tasks_open: bool,
    /// The page, kept while the runtime stays so it reopens on its scope.
    pub(crate) scheduled_tasks: Option<Entity<crate::scheduled_tasks::ScheduledTasks>>,
    /// Whether the composer shows the short placeholder; unknown until the
    /// first frame measures it.
    pub(crate) composer_placeholder_compact: Option<bool>,
    playback: HashMap<SessionId, Task<()>>,
    /// Running state per session, from the live channel only.
    pub(crate) live_status: HashMap<SessionId, LiveStatus>,
    pub(crate) transcript_clock: crate::transcript_clock::TranscriptClock,
    pub(crate) message_actions: crate::message_actions::MessageActions,
    pub(crate) transcript_search: Option<crate::transcript_search::TranscriptSearch>,
    /// Present when the source is a server.
    pub(crate) connection: Option<Connection>,
    pub(crate) retired_networks: Vec<(RuntimeId, crate::network::Network)>,
    pub(crate) authentication: crate::authentication::Authentication,
    pub(crate) host_manager: Option<Entity<crate::host_manager::HostManager>>,
    pub(crate) settings: Option<Entity<crate::settings_view::Settings>>,
    pub(crate) host_recovery: Option<crate::host_recovery::HostRecovery>,
    pub(crate) host_close: Option<Task<()>>,
    pub(crate) appearance_close: Option<Task<()>>,
    pub(crate) general_close: Option<Task<()>>,
    /// The composer's catalogs and defaults; empty for synthetic data.
    pub(crate) send_catalog: SendCatalog,
    pub(crate) effort_preferences: crate::effort_preferences::EffortPreferences,
    pub(crate) effort_close: Option<Task<()>>,
    pub(crate) composer_selections: openchamber_native_core::composer_selection::ComposerSelections,
    pub(crate) model_picker: crate::composer_picker::Picker,
    pub(crate) agent_picker: crate::composer_picker::Picker,
    pub(crate) effort_picker: crate::composer_picker::Picker,
    pub(crate) send_attempts: crate::send_attempt::SendAttempts,
    pub(crate) blocking: crate::blocking_requests::BlockingRequests,
    pub(crate) permission_modes: crate::permission_modes::PermissionModes,
    pub(crate) queue: crate::message_queue::QueueState,
    pub(crate) queue_storage: crate::queue_storage::QueueStorage,
    pub(crate) starters: crate::starters::Starters,
    pub(crate) goals: crate::session_goal::Goals,
    pub(crate) reviews: crate::review_flow::Reviews,
    /// The session a first Escape armed a stop for, until when.
    pub(crate) abort_primed: Option<(SessionId, Instant)>,
    pub(crate) recovery: crate::send_recovery::RecoveryState,
    pub(crate) persistence: crate::state_persistence::StatePersistence,
    pub(crate) mutation_tasks: HashMap<crate::send_attempt::SessionKey, Task<()>>,
    /// Interrupts, apart from send chains: Stop can start while a send of the
    /// same session is still rejecting the requests it supersedes.
    pub(crate) abort_tasks: HashMap<crate::send_attempt::SessionKey, Task<()>>,
    pub(crate) message_ids: crate::message_id::MessageIds,
    pub(crate) session_ids: crate::session_id::SessionIds,
    pub(crate) creations: crate::create_attempt::CreateAttempts,
    pub(crate) creation_tasks: HashMap<crate::send_attempt::SessionKey, Task<()>>,
    pub(crate) navigation_revision: u64,
    pub(crate) session_admission: crate::session_admission::SessionAdmission,
    pub(crate) code_wrap_state: CodeWrapState,
    pub(crate) code_wrap_save_task: Option<Task<()>>,
    pub(crate) provider_logos: ProviderLogos,
    pub(crate) enterprise: crate::enterprise::PolicyMonitor,
    pub(crate) project_readouts: ProjectReadouts,
    /// The context panel's Files, Git and Diff surfaces.
    pub(crate) panels: crate::files_view::Panels,
    /// Set while the new window's content is held at its requested size.
    pub(crate) initial_size: Option<InitialSize>,
    /// The docked composer's slot, measured while painting.
    pub(crate) composer_slot: Rc<Cell<ComposerSlot>>,
    prompt_sequence: usize,
    pub(crate) focus_handle: FocusHandle,
    pending_switch: Option<Instant>,
    /// Markers for the next painted frame.
    pending_markers: Vec<(Marker, Option<Instant>, Option<usize>)>,
    startup_recorded: bool,
    composer_usable_recorded: bool,
    /// Whether a complete session list was painted yet.
    pub(crate) sessions_shown: bool,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub(crate) fn new(source: Source, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut sessions = SessionIndex::new();
        let (topology, runtime, connection, host_recovery) = match source {
            Source::Synthetic(size) => {
                let data = SyntheticWorkspace::new(size);
                let ticket = sessions.begin_load();
                let _ = sessions.complete_load(ticket, data.sessions);
                let topology = Topology {
                    projects: data.projects,
                    worktrees: data.worktrees,
                    chat_roots: Some(data.chat_roots),
                };
                (topology, RuntimeId::from("synthetic"), None, None)
            }
            Source::Server(connection) => {
                let runtime = RuntimeId::new(connection.config.endpoint.as_str().to_owned());
                (Topology::default(), runtime, Some(*connection), None)
            }
            Source::HostRecovery { store, error } => (
                Topology::default(),
                RuntimeId::from("host-recovery"),
                None,
                Some(crate::host_recovery::HostRecovery::new(store, error, cx)),
            ),
        };
        let placeholder = locale::strings(cx).get("chat.chatInput.placeholder.chat");
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .observe_committed_edits()
                .auto_grow(1, 10)
                .submit_on_enter(true)
                .placeholder(placeholder)
        });
        let subscription = cx.subscribe_in(&composer, window, Self::on_composer_event);
        let committed_subscription =
            cx.subscribe_in(&composer, window, Self::on_committed_composer_edit);

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
            pane_sizes: Default::default(),
            work_status_open: true,
            work_status_overlay: false,
            terminal_panel: None,
            terminal_open: false,
            prompt_panel: None,
            prompt_hover_task: None,
            prompt_hover_suppressed: false,
            work_status_context_open: false,
            work_status_telemetry_open: true,
            title_drag_armed: false,
            selected: None,
            open_sessions: HashMap::new(),
            drafts: DraftStore::new(),
            attachments: Default::default(),
            attachment_preparations: Vec::new(),
            attachment_staging: Default::default(),
            attachment_retirement: Default::default(),
            attachment_storage: Default::default(),
            attachment_storage_close: None,
            retired_attachment_blobs: Vec::new(),
            scroll_memory: ScrollMemory::new(),
            composer,
            composer_documents: Default::default(),
            attachment_input: Default::default(),
            autocomplete: Default::default(),
            session_references: Default::default(),
            composer_key: None,
            draft_previews: HashMap::new(),
            draft_rows: HashMap::new(),
            draft_projects: HashMap::new(),
            project_chooser: None,
            session_menu: None,
            app_menu: Default::default(),
            session_rename: None,
            session_management: Default::default(),
            branching: Default::default(),
            archive_open: false,
            source_board_open: false,
            source_board: None,
            scheduled_tasks_open: false,
            scheduled_tasks: None,
            composer_placeholder_compact: None,
            playback: HashMap::new(),
            live_status: HashMap::new(),
            transcript_clock: Default::default(),
            message_actions: Default::default(),
            transcript_search: None,
            connection,
            retired_networks: Vec::new(),
            authentication: crate::authentication::Authentication::new(window, cx),
            host_manager: None,
            settings: None,
            host_recovery,
            host_close: None,
            appearance_close: None,
            general_close: None,
            send_catalog: SendCatalog::default(),
            effort_preferences: Default::default(),
            effort_close: None,
            composer_selections:
                openchamber_native_core::composer_selection::ComposerSelections::default(),
            model_picker: crate::composer_picker::Picker::new(
                crate::composer_picker::PickerKind::Model,
                window,
                cx,
            ),
            agent_picker: crate::composer_picker::Picker::new(
                crate::composer_picker::PickerKind::Agent,
                window,
                cx,
            ),
            effort_picker: crate::composer_picker::Picker::new(
                crate::composer_picker::PickerKind::Effort,
                window,
                cx,
            ),
            send_attempts: crate::send_attempt::SendAttempts::default(),
            blocking: crate::blocking_requests::BlockingRequests::default(),
            permission_modes: crate::permission_modes::PermissionModes::default(),
            queue: Default::default(),
            queue_storage: Default::default(),
            starters: Default::default(),
            goals: Default::default(),
            reviews: Default::default(),
            abort_primed: None,
            recovery: crate::send_recovery::RecoveryState::default(),
            persistence: crate::state_persistence::StatePersistence::default(),
            mutation_tasks: HashMap::new(),
            abort_tasks: HashMap::new(),
            message_ids: crate::message_id::MessageIds::default(),
            session_ids: crate::session_id::SessionIds::default(),
            creations: crate::create_attempt::CreateAttempts::default(),
            creation_tasks: HashMap::new(),
            navigation_revision: 0,
            session_admission: crate::session_admission::SessionAdmission::default(),
            code_wrap_state: CodeWrapState::default(),
            code_wrap_save_task: None,
            provider_logos: ProviderLogos::default(),
            enterprise: crate::enterprise::PolicyMonitor::default(),
            project_readouts: ProjectReadouts::default(),
            panels: crate::files_view::Panels::default(),
            initial_size: None,
            composer_slot: Rc::default(),
            prompt_sequence: 0,
            focus_handle: cx.focus_handle(),
            pending_switch: None,
            pending_markers: Vec::new(),
            startup_recorded: false,
            composer_usable_recorded: false,
            sessions_shown: false,
            _subscriptions: vec![subscription, committed_subscription],
        };
        workspace.rebuild_sidebar();
        if let Some(appearance) = cx.try_global::<crate::appearance_view::AppearanceOwner>() {
            let appearance = appearance.0.clone();
            let mut previous = appearance.read(cx).preferences().clone();
            workspace._subscriptions.push(cx.observe_in(
                &appearance,
                window,
                move |workspace, appearance, window, cx| {
                    let preferences = appearance.read(cx).preferences().clone();
                    let metrics_changed = previous.font_size != preferences.font_size
                        || previous.ui_font != preferences.ui_font
                        || previous.mono_font != preferences.mono_font
                        || previous.locale != preferences.locale;
                    let locale_changed = previous.locale != preferences.locale;
                    previous = preferences;
                    if metrics_changed {
                        workspace.sidebar_list.remeasure();
                        for open in workspace.open_sessions.values() {
                            open.list.remeasure();
                        }
                        workspace.composer_slot.set(ComposerSlot::default());
                    }
                    if locale_changed {
                        workspace.refresh_input_language(window, cx);
                    }
                    if metrics_changed {
                        cx.notify();
                    }
                },
            ));
        }
        workspace._subscriptions.push(cx.subscribe_in(
            &workspace.authentication.password,
            window,
            Self::on_authentication_input,
        ));
        // The window opens on a new session, as the web client does.
        workspace.composer_key = workspace.new_session_draft_key();
        workspace.start_persistence(window, cx);
        let window_handle = window.window_handle();
        workspace
            ._subscriptions
            .push(cx.on_request_quit_in(window, |workspace, window, cx| {
                workspace.request_close(window, cx);
                false
            }));
        workspace
            ._subscriptions
            .push(cx.on_app_quit(move |workspace, cx| {
                let general = workspace.take_general_flush(cx);
                let appearance = workspace.take_appearance_flush(window_handle, cx);
                let hosts = workspace.take_profile_quit_flush(cx);
                let efforts = workspace.take_effort_quit_flush(cx);
                let flushed = workspace.flush_before_quit(cx);
                let active_network = workspace.connection.take().map(Connection::into_network);
                let retired_networks = std::mem::take(&mut workspace.retired_networks);
                async move {
                    if let Some(general) = general {
                        general.await;
                    }
                    if let Some(appearance) = appearance {
                        appearance.await;
                    }
                    if let Some(hosts) = hosts {
                        hosts.await;
                    }
                    if let Some(efforts) = efforts {
                        efforts.await;
                    }
                    if let Some(flushed) = flushed {
                        let _ = flushed.await;
                    }
                    drop(active_network);
                    drop(retired_networks);
                }
            }));
        let owner = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            owner
                .update(cx, |workspace, cx| workspace.request_close(window, cx))
                .is_err()
        });
        if let Some(recovery) = &workspace.host_recovery {
            recovery.focus(window, cx);
        } else {
            workspace.focus_composer(window, cx);
        }
        if workspace.connection.is_some() {
            workspace.connect(window, cx);
        }
        workspace
    }

    /// Updates retained placeholder presentation without replacing editor content.
    fn refresh_input_language(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer_placeholder_compact = None;
        let password = locale::strings(cx).get("sessionAuth.password.placeholder");
        self.authentication
            .password
            .update(cx, |input, cx| input.set_placeholder(password, window, cx));
        if let Some(manager) = &self.host_manager {
            manager.update(cx, |manager, cx| manager.refresh_language(window, cx));
        }
        if let Some(rename) = &self.session_rename {
            let placeholder = locale::strings(cx).get("sessions.sidebar.session.menu.rename");
            rename.input.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx)
            });
        }
        let fallback = locale::strings(cx).get("chat.formCard.yourAnswer");
        for draft in self.blocking.drafts.values() {
            for field in &draft.form.fields {
                if matches!(
                    &field.kind,
                    openchamber_native_core::form::FormFieldKind::String {
                        placeholder: Some(_),
                        ..
                    }
                ) {
                    continue;
                }
                if let Some(input) = draft.inputs.get(&field.key) {
                    input.update(cx, |input, cx| {
                        input.set_placeholder(fallback.clone(), window, cx)
                    });
                }
            }
        }
    }

    /// Whether the workspace shows synthetic data.
    pub fn is_synthetic(&self) -> bool {
        self.connection.is_none() && self.host_recovery.is_none()
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
        // Pins the server has not answered for yet show at once.
        let pinned = self.session_management.pins.overlay(self.sessions.active());
        let (mut items, mut spacing) = build_sidebar_items(
            &self.topology,
            pinned.as_deref().unwrap_or(self.sessions.active()),
            self.sessions.archived(),
            &self.sidebar_state,
            &mut self.ordering,
            self.send_catalog.settings.as_ref(),
        );
        // A search matches sessions; drafts stay out of it.
        if self.sidebar_state.search.is_none() {
            self.append_draft_rows(&mut items, &mut spacing);
        }
        let elapsed = started.elapsed();
        let top = self.sidebar_list.logical_scroll_top();
        let anchor = self
            .sidebar_items
            .get(top.item_ix)
            .map(|item| item.key().to_owned());
        let count = items.len();
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
        let directory = self.draft_projects.get(&self.runtime);
        DraftKey::new(
            self.runtime.clone(),
            directory.map_or(NEW_SESSION_DIRECTORY, String::as_str),
            DraftTarget::NewSession(DraftId::from(if directory.is_some() {
                PROJECT_DRAFT_ID
            } else {
                UNASSIGNED_DRAFT_ID
            })),
        )
    }

    /// Changes only the client draft target, after checking registered topology.
    pub(crate) fn choose_draft_project(
        &mut self,
        directory: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected.is_some()
            || !self
                .topology
                .projects
                .iter()
                .any(|project| project.path == directory)
        {
            return;
        }
        self.save_composer_draft(cx);
        self.draft_projects
            .insert(self.runtime.clone(), directory.to_owned());
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        self.composer_key = self.new_session_draft_key();
        self.restore_composer_draft(window, cx);
        self.ensure_send_catalog(cx);
        self.focus_composer(window, cx);
        cx.notify();
    }

    /// Leaves the selected session for the new-session view, keeping both
    /// drafts and the session's reading position.
    pub fn show_new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        self.archive_open = false;
        self.source_board_open = false;
        self.scheduled_tasks_open = false;
        self.close_selected_session(window, cx);
        self.focus_composer(window, cx);
        cx.notify();
    }

    /// Closes the selected session, keeping its draft and reading position,
    /// without navigating or moving focus.
    pub(crate) fn close_selected_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        if self.selected.is_some() {
            self.save_composer_draft(cx);
            self.save_scroll_position();
            self.selected = None;
            self.blocking.presented = None;
            self.composer_key = self.new_session_draft_key();
            self.restore_composer_draft(window, cx);
        }
    }

    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        self.save_layout(cx);
        cx.notify();
    }

    pub fn toggle_work_status(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.work_status_open && !self.work_status_fits(window) {
            self.work_status_overlay = !self.work_status_overlay;
        } else {
            self.work_status_open = !self.work_status_open;
            self.work_status_overlay = false;
        }
        cx.notify();
    }

    /// Selects a session: saves the outgoing draft and position under their
    /// captured identities, then restores the incoming ones before paint.
    pub fn select_session(&mut self, id: SessionId, window: &mut Window, cx: &mut Context<Self>) {
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        if !self.gate_session_open(&id, window, cx) {
            return;
        }
        self.select_admitted_session(id, window, cx);
    }

    /// Completes navigation only after the destination has authoritative admission.
    pub(crate) fn select_admitted_session(
        &mut self,
        id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.session_admitted(&id) {
            return;
        }
        // Selecting a session always leaves a full-page surface.
        self.archive_open = false;
        self.source_board_open = false;
        self.scheduled_tasks_open = false;
        self.prompt_panel = None;
        self.prompt_hover_task = None;
        self.prompt_hover_suppressed = false;
        if self.selected.as_ref() == Some(&id) {
            self.focus_composer(window, cx);
            return;
        }
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
        self.open_btw_fork(cx);
        self.composer_key = next_key;
        self.restore_composer_draft(window, cx);
        self.focus_composer(window, cx);
        self.pending_switch = Some(started);
        self.refresh_blocking_requests(window, cx);
        self.acknowledge_visible_form(cx);
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
                    let prefix = format!("{project_id}:");
                    state
                        .visible_count_by_container
                        .retain(|key, _| !key.starts_with(&prefix));
                }
            }
            SidebarItem::Group { group_key, .. } => {
                if !state.collapsed_groups.remove(group_key) {
                    state.collapsed_groups.insert(group_key.clone());
                }
            }
            SidebarItem::Worktree { group_key, .. } => {
                if state.expanded_works.remove(group_key) {
                    state.visible_count_by_container.remove(group_key);
                } else {
                    state.expanded_works.insert(group_key.clone());
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

    pub(crate) fn ensure_open(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        if !self.session_admitted(id) {
            return;
        }
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
        let selection =
            cx.new(|cx| crate::transcript_selection::TranscriptSelection::new(&transcript, cx));
        self.open_sessions.insert(
            id.clone(),
            OpenSession {
                history_generation: 0,
                transcript,
                selection,
                list,
                history: History::Complete,
                older: None,
                earlier: EarlierHistory::Idle,
                rail_current: None,
                turn_stats: Cell::new(None),
                parts: crate::part_state::PartState::default(),
            },
        );
        if !self.is_synthetic() {
            self.load_history(id, cx);
        }
    }

    pub(crate) fn save_scroll_position(&mut self) {
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

    pub(crate) fn save_composer_draft(&mut self, cx: &App) {
        if self.save_compound_composer(cx) {
            return;
        }
        let Some(key) = self.composer_key.clone() else {
            return;
        };
        // Keep potentially large context payloads with their existing owner while
        // rebasing text anchors and recording the editor's selection.
        let mut draft = self.drafts.take(&key).unwrap_or_default();
        let composer = self.composer.read(cx);
        let (text, selection) = composer.committed_snapshot();
        let text = text.to_string();
        let changed = draft.text != text;
        draft.edit_text(text, selection);
        if changed {
            self.record_native_composer_change(&key);
        }
        self.save_draft_preview(&key, &draft, cx);
        self.drafts.save(key, draft);
    }

    /// Text widgets own only text and caret state; semantic content stays with
    /// the captured draft even while an asynchronous preparation updates it.
    pub(crate) fn composer_draft(&self, cx: &App) -> Draft {
        let composer = self.composer.read(cx);
        let mut draft = self
            .composer_key
            .as_ref()
            .and_then(|key| self.drafts.get(key))
            .cloned()
            .unwrap_or_default();
        if self
            .composer_key
            .as_ref()
            .is_some_and(|owner| self.composer_documents.contains(owner))
        {
            return draft;
        }
        let (text, selection) = composer.committed_snapshot();
        draft.edit_text(text.to_string(), selection);
        draft
    }

    pub(crate) fn composer_has_sendable_content(&self, cx: &App) -> bool {
        if let Some(sendable) = self.compound_composer_sendable() {
            return sendable;
        }
        !self.composer.read(cx).value().trim().is_empty()
            || self
                .composer_key
                .as_ref()
                .and_then(|key| self.drafts.get(key))
                .is_some_and(|draft| {
                    !draft.content.files.is_empty() || !draft.content.context.is_empty()
                })
    }

    pub(crate) fn consume_draft_capture(
        &mut self,
        owner: &DraftKey,
        captured: &Draft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_composer_draft(cx);
        let Some(mut current) = self.drafts.get(owner).cloned() else {
            return;
        };
        if !current.consume_captured(captured) {
            return;
        }
        self.drafts.save(owner.clone(), current);
        if self.composer_key.as_ref() == Some(owner) {
            self.restore_composer_draft(window, cx);
        }
    }

    pub(crate) fn restore_composer_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_autocomplete(cx);
        if self.restore_compound_composer(window, cx) {
            return;
        }
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
        self.sync_draft_preview(cx);
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
        // CommittedInputEdit owns persistence. IME preedit and a batched undo
        // must never be captured through an intermediate Change notification.
        if matches!(event, InputEvent::CompositionEnded) {
            self.on_composer_composition_ended(window, cx);
        } else if let InputEvent::PressEnter {
            shift: false,
            secondary,
        } = event
        {
            self.submit_from_enter(*secondary, window, cx);
        }
    }

    /// Sends to a selected session or durably creates the selected project draft.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_synthetic() {
            // `/handoff-review`, `/fork`, `/btw` and an open side
            // conversation take the text.
            if self.run_review_command(window, cx) || self.run_branch_command(window, cx) {
                return;
            }
            if self.btw_takes_composer() {
                self.send_btw_follow_up(window, cx);
                return;
            }
            if !self.session_references_ready(
                Box::new(|this, window, cx| this.submit(window, cx)),
                window,
                cx,
            ) {
                return;
            }
            if self.selected.is_some() {
                self.submit_connected(window, cx);
            } else {
                self.submit_new_session(window, cx);
            }
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
                    synthetic: None,
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
                    synthetic: None,
                },
                parts: vec![Part::text(part_id.clone(), String::new())],
            },
        ] {
            let change = open.transcript.push(message);
            crate::server_sync::apply_list_change(open, &change, cx);
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

    /// Shows code wrapped or not; every transcript row is measured again,
    /// since code blocks change height.
    pub(crate) fn show_code_wrap(&mut self, wrap: bool, cx: &mut Context<Self>) {
        if cx.try_global::<CodeWrap>().copied().unwrap_or_default() == CodeWrap(wrap) {
            return;
        }
        cx.set_global(CodeWrap(wrap));
        for open in self.open_sessions.values() {
            open.list.remeasure();
        }
        cx.notify();
    }

    /// Stops the selected session's stream, keeping the text received so far.
    pub fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_synthetic() {
            self.abort_connected(window, cx);
            return;
        }
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
        if matches!(change, TranscriptChange::Updated { .. }) {
            crate::server_sync::apply_list_change(open, &change, cx);
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
        }
        if !self.composer_usable_recorded && self.composer_editable() {
            self.composer_usable_recorded = true;
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
    settings: Option<&ServerSettings>,
) -> (Vec<SidebarItem>, Vec<Option<SectionSpacing>>) {
    let inputs = OwnershipInputs {
        sessions: active,
        archived_sessions: archived,
        projects: &topology.projects,
        worktrees: &topology.worktrees,
        surface: Surface::Desktop,
        ..OwnershipInputs::default()
    };
    let collection = SidebarCollection::build(&inputs, topology.chat_roots.as_ref(), ordering);
    let ownership = SessionOwnershipIndex::build_for_sidebar(&inputs, &collection);
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
        // `partitionSidebarSessions`: a `/btw` fork never reaches a project.
        let mut sessions: Vec<&SessionRecord> = ownership
            .sessions_by_project()
            .get(&project.id)
            .map(|bucket| {
                bucket
                    .iter()
                    .copied()
                    .filter(|session| !session.is_btw())
                    .collect()
            })
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
    let mut chat_group = grouping::build_chat_group(&collection, topology.chat_roots.as_ref());
    if let Some(query) = &state.search {
        crate::sidebar_search::filter_sections(&mut sections, chat_group.as_mut(), query);
    }
    let (pinned, work) = sidebar_rows::zone_items(&sections, chat_group.as_ref(), ordering);
    let timeline = sidebar_rows::timeline_items(&sections, ordering);
    let group_status = HashMap::new();
    let model = sidebar_rows::build(
        &SidebarInputs {
            view_mode: settings
                .and_then(|settings| settings.sidebar_view)
                .unwrap_or_default(),
            detailed_zone_rows: false,
            keep_work_in_place: settings
                .and_then(|settings| settings.keep_work_in_place)
                .unwrap_or(false),
            chat_group: chat_group.as_ref().filter(|_| {
                settings
                    .and_then(|settings| settings.show_chats)
                    .unwrap_or(true)
            }),
            pinned: if settings
                .and_then(|settings| settings.show_pinned)
                .unwrap_or(true)
            {
                &pinned
            } else {
                &[]
            },
            work: if settings
                .and_then(|settings| settings.show_work)
                .unwrap_or(true)
                && settings
                    .and_then(|settings| settings.work_enabled)
                    .unwrap_or(true)
            {
                &work
            } else {
                &[]
            },
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
            None,
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

    #[test]
    fn sidebar_preferences_control_zone_visibility_and_time_grouping() {
        use openchamber_native_core::sidebar_rows::{ActivityKey, SidebarViewMode};
        let data = SyntheticWorkspace::new(WorkloadSize::Small);
        let topology = Topology {
            projects: data.projects,
            worktrees: data.worktrees,
            chat_roots: Some(data.chat_roots),
        };
        let settings = ServerSettings {
            show_chats: Some(false),
            show_pinned: Some(false),
            show_work: Some(false),
            ..ServerSettings::default()
        };
        let (project_items, _) = build_sidebar_items(
            &topology,
            &data.sessions,
            &[],
            &SidebarState::default(),
            &mut SessionOrdering::new(),
            Some(&settings),
        );
        assert!(
            !project_items
                .iter()
                .any(|item| matches!(item, SidebarItem::Zone { .. })),
            "hidden zones have no rows"
        );
        let by_time = ServerSettings {
            sidebar_view: Some(SidebarViewMode::WorksByTime),
            ..settings
        };
        let (time_items, _) = build_sidebar_items(
            &topology,
            &data.sessions,
            &[],
            &SidebarState::default(),
            &mut SessionOrdering::new(),
            Some(&by_time),
        );
        assert!(
            time_items.iter().any(|item| matches!(
                item,
                SidebarItem::Zone {
                    activity: ActivityKey::Timeline,
                    ..
                }
            )),
            "time grouping has one shared list"
        );
        assert!(
            !time_items
                .iter()
                .any(|item| matches!(item, SidebarItem::Project { .. })),
            "time grouping has no project headers"
        );
    }
}
