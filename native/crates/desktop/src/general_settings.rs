//! Server-backed Settings preferences, scoped to a captured server: General's
//! Files and Terminal fields and the implemented Chat and Sessions fields.
//! Each runtime serializes its writes. Failed fields retain their chosen value
//! until an explicit retry; other fields can still save.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::component::IndexPath;
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::{
    AppContext, Context, Entity, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, Role, ScrollAnchor, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_core::message_queue::FollowUpBehavior;
use openchamber_native_core::permission_mode::PermissionMode;
use openchamber_native_transport::client::{
    ServerClient, ServerSettings, SettingsChanges, WorkStatusSectionList,
};
use openchamber_native_transport::dictation::SttProvider;
use openchamber_native_transport::notification::NotificationTemplates;
use openchamber_native_transport::terminal::{TerminalApi, TerminalShell};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::context_panel::work_status_sections;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::network::{Network, NetworkTask};
use crate::theme::Palette;
use crate::workspace::Workspace;

const AUTO_SAVE: usize = 0;
const CLOSE_ON_EXIT: usize = 1;
const SHELL: usize = 2;
pub(crate) const PROMPT_NAVIGATOR: usize = 3;
pub(crate) const FOLLOW_UP: usize = 4;
pub(crate) const DRAFT_STARTERS: usize = 5;
pub(crate) const PERMISSION_DEFAULT: usize = 6;
pub(crate) const SHOW_CHATS: usize = 7;
pub(crate) const WORK_ENABLED: usize = 8;
pub(crate) const DELETION_DIALOG: usize = 9;
pub(crate) const SUBAGENT_PROMPTS: usize = 10;
pub(crate) const NOTIFY_ENABLED: usize = 11;
pub(crate) const NOTIFY_ALWAYS: usize = 12;
pub(crate) const NOTIFY_COMPLETION: usize = 13;
pub(crate) const NOTIFY_PULL_REQUESTS: usize = 14;
pub(crate) const NOTIFY_SUBTASKS: usize = 15;
pub(crate) const NOTIFY_ERROR: usize = 16;
pub(crate) const NOTIFY_QUESTION: usize = 17;
pub(crate) const NOTIFY_TEMPLATES: usize = 18;
pub(crate) const DICTATION_ENABLED: usize = 19;
pub(crate) const DICTATION_AUTO_SEND: usize = 20;
pub(crate) const STT_PROVIDER: usize = 21;
pub(crate) const STT_SERVER_URL: usize = 22;
pub(crate) const STT_MODEL: usize = 23;
pub(crate) const STT_ENDPOINT_PATH: usize = 24;
pub(crate) const STT_LOCAL_MODEL: usize = 25;
pub(crate) const STT_LANGUAGE: usize = 26;
pub(crate) const STT_CORRECT: usize = 27;
pub(crate) const ARCHIVE_ON_DONE: usize = 28;
pub(crate) const MERGED_CLEANUP: usize = 29;
const WORK_STATUS_ORDER: usize = 30;
const WORK_STATUS_HIDDEN: usize = 31;
const WORK_STATUS_EXPLICIT: usize = 32;
const FIELDS: usize = 33;
const WORK_STATUS_FIELDS: [usize; 3] =
    [WORK_STATUS_ORDER, WORK_STATUS_HIDDEN, WORK_STATUS_EXPLICIT];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    Boolean(bool),
    Shell(String),
    FollowUp(FollowUpBehavior),
    Mode(PermissionMode),
    Text(String),
    Templates(NotificationTemplates),
    Provider(SttProvider),
    Sections(Vec<String>),
}

type Values = [Option<Choice>; FIELDS];
type WriteResult = Option<Values>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Write {
    revisions: [u64; FIELDS],
    values: Values,
}

#[derive(Debug)]
struct State {
    values: [Choice; FIELDS],
    known: [bool; FIELDS],
    revisions: [u64; FIELDS],
    pending: [bool; FIELDS],
    failed: [bool; FIELDS],
    writing: Option<Write>,
}

impl Default for State {
    /// Unknown fields show the defaults their consumers apply.
    fn default() -> Self {
        Self {
            values: [
                Choice::Boolean(true),
                Choice::Boolean(true),
                Choice::Shell("auto".into()),
                Choice::Boolean(true),
                Choice::FollowUp(FollowUpBehavior::default()),
                Choice::Boolean(true),
                Choice::Mode(PermissionMode::default()),
                Choice::Boolean(true),
                Choice::Boolean(true),
                Choice::Boolean(true),
                // Subagent prompting stays off until settings are read.
                Choice::Boolean(false),
                // Notifications: off, background only, every event on.
                Choice::Boolean(false),
                Choice::Boolean(false),
                Choice::Boolean(true),
                Choice::Boolean(true),
                Choice::Boolean(true),
                Choice::Boolean(true),
                Choice::Boolean(true),
                Choice::Templates(NotificationTemplates::default()),
                // Dictation, as `useConfigStore` starts it.
                Choice::Boolean(true),
                Choice::Boolean(false),
                Choice::Provider(SttProvider::Local),
                Choice::Text("http://localhost:8001/v1".into()),
                Choice::Text("deepdml/faster-whisper-large-v3-turbo-ct2".into()),
                Choice::Text(String::new()),
                Choice::Text(String::new()),
                Choice::Text(String::new()),
                Choice::Boolean(false),
                // Both finished-worktree switches are off until chosen.
                Choice::Boolean(false),
                Choice::Boolean(false),
                Choice::Sections(work_status_sections::sanitize_order(&[])),
                Choice::Sections(Vec::new()),
                Choice::Boolean(false),
            ],
            revisions: [0; FIELDS],
            known: [false; FIELDS],
            pending: [false; FIELDS],
            failed: [false; FIELDS],
            writing: None,
        }
    }
}

impl State {
    fn change(&mut self, field: usize, value: Choice) {
        self.values[field] = value;
        self.known[field] = true;
        self.revisions[field] += 1;
        self.pending[field] = true;
        self.failed[field] = false;
    }

    fn begin(&mut self) -> Option<Write> {
        if self.writing.is_some() {
            return None;
        }
        let write = self.take_pending()?;
        self.writing = Some(write.clone());
        Some(write)
    }

    fn take_pending(&mut self) -> Option<Write> {
        if !self.pending.iter().any(|pending| *pending) {
            return None;
        }
        let write = Write {
            revisions: self.revisions,
            values: std::array::from_fn(|field| {
                std::mem::take(&mut self.pending[field]).then(|| self.values[field].clone())
            }),
        };
        Some(write)
    }

    fn finish(&mut self, write: &Write, response: WriteResult) {
        if self.writing.as_ref() != Some(write) {
            return;
        }
        self.writing = None;
        for field in 0..FIELDS {
            if let Some(value) = &write.values[field]
                && self.revisions[field] == write.revisions[field]
            {
                self.failed[field] =
                    response.as_ref().and_then(|fields| fields[field].as_ref()) != Some(value);
            }
        }
    }

    /// Whether the field holds a choice the server has not confirmed.
    fn unsettled(&self, field: usize) -> bool {
        self.pending[field]
            || self.failed[field]
            || self
                .writing
                .as_ref()
                .is_some_and(|write| write.values[field].is_some())
    }

    fn read_revisions(&self) -> [Option<u64>; FIELDS] {
        std::array::from_fn(|field| (!self.unsettled(field)).then_some(self.revisions[field]))
    }

    fn adopt(&mut self, revisions: [Option<u64>; FIELDS], values: Values) {
        for (field, value) in values.into_iter().enumerate() {
            if Some(self.revisions[field]) == revisions[field]
                && !self.unsettled(field)
                && let Some(value) = value
            {
                self.values[field] = value;
                self.known[field] = true;
            }
        }
    }

    fn retry(&mut self) {
        for field in 0..FIELDS {
            if std::mem::take(&mut self.failed[field]) {
                self.pending[field] = true;
            }
        }
    }

    fn boolean(&self, field: usize) -> bool {
        matches!(self.values[field], Choice::Boolean(true))
    }

    fn shell(&self) -> &str {
        match &self.values[SHELL] {
            Choice::Shell(shell) => shell,
            _ => unreachable!("the shell field only accepts shell choices"),
        }
    }

    fn follow_up(&self) -> FollowUpBehavior {
        match self.values[FOLLOW_UP] {
            Choice::FollowUp(behavior) => behavior,
            _ => unreachable!("the follow-up field only accepts follow-up choices"),
        }
    }

    fn permission_mode(&self) -> PermissionMode {
        match self.values[PERMISSION_DEFAULT] {
            Choice::Mode(mode) => mode,
            _ => unreachable!("the permission field only accepts modes"),
        }
    }

    pub(crate) fn text(&self, field: usize) -> &str {
        match &self.values[field] {
            Choice::Text(text) => text,
            _ => unreachable!("text fields only accept text"),
        }
    }

    fn provider(&self) -> SttProvider {
        match self.values[STT_PROVIDER] {
            Choice::Provider(provider) => provider,
            _ => unreachable!("the provider field only accepts providers"),
        }
    }

    fn sections(&self, field: usize) -> &[String] {
        match &self.values[field] {
            Choice::Sections(sections) => sections,
            _ => unreachable!("section fields only accept section lists"),
        }
    }

    /// Known choices, or with `unsettled_only` only those the server has not confirmed.
    fn preferences(&self, unsettled_only: bool) -> GeneralPreferences {
        let include = |field| self.known[field] && (!unsettled_only || self.unsettled(field));
        let boolean = |field| include(field).then(|| self.boolean(field));
        GeneralPreferences {
            auto_save_enabled: boolean(AUTO_SAVE),
            terminal_close_on_exit: boolean(CLOSE_ON_EXIT),
            terminal_shell: include(SHELL).then(|| self.shell().to_owned()),
            prompt_navigator_enabled: boolean(PROMPT_NAVIGATOR),
            follow_up_behavior: include(FOLLOW_UP).then(|| self.follow_up()),
            draft_starters_visible: boolean(DRAFT_STARTERS),
            permission_default_mode: include(PERMISSION_DEFAULT).then(|| self.permission_mode()),
            show_chats: boolean(SHOW_CHATS),
            work_enabled: boolean(WORK_ENABLED),
            show_deletion_dialog: boolean(DELETION_DIALOG),
            allow_prompting_subagent_sessions: boolean(SUBAGENT_PROMPTS),
            dictation_enabled: boolean(DICTATION_ENABLED),
            dictation_auto_send: boolean(DICTATION_AUTO_SEND),
            stt_provider: include(STT_PROVIDER).then(|| self.provider()),
            stt_server_url: include(STT_SERVER_URL).then(|| self.text(STT_SERVER_URL).to_owned()),
            stt_model: include(STT_MODEL).then(|| self.text(STT_MODEL).to_owned()),
            stt_endpoint_path: include(STT_ENDPOINT_PATH)
                .then(|| self.text(STT_ENDPOINT_PATH).to_owned()),
            stt_local_model: include(STT_LOCAL_MODEL)
                .then(|| self.text(STT_LOCAL_MODEL).to_owned()),
            stt_language: include(STT_LANGUAGE).then(|| self.text(STT_LANGUAGE).to_owned()),
            stt_correct_transcript: boolean(STT_CORRECT),
            worktree_archive_on_done: boolean(ARCHIVE_ON_DONE),
            merged_worktree_cleanup: boolean(MERGED_CLEANUP),
            work_status_section_order: include(WORK_STATUS_ORDER)
                .then(|| WorkStatusSectionList::Valid(self.sections(WORK_STATUS_ORDER).to_vec())),
            work_status_hidden_sections: include(WORK_STATUS_HIDDEN)
                .then(|| WorkStatusSectionList::Valid(self.sections(WORK_STATUS_HIDDEN).to_vec())),
            work_status_hidden_sections_explicit: boolean(WORK_STATUS_EXPLICIT),
        }
    }
}

fn fields(settings: &ServerSettings) -> Values {
    let [order, hidden, explicit] = work_status_fields(settings);
    [
        settings.auto_save_enabled.map(Choice::Boolean),
        settings.terminal_close_on_exit.map(Choice::Boolean),
        settings.terminal_shell.clone().map(Choice::Shell),
        settings.prompt_navigator_enabled.map(Choice::Boolean),
        settings.follow_up_behavior.map(Choice::FollowUp),
        settings.draft_starters_visible.map(Choice::Boolean),
        settings.permission_default_mode.map(Choice::Mode),
        settings.show_chats.map(Choice::Boolean),
        settings.work_enabled.map(Choice::Boolean),
        settings.show_deletion_dialog.map(Choice::Boolean),
        settings
            .allow_prompting_subagent_sessions
            .map(Choice::Boolean),
        settings.native_notifications_enabled.map(Choice::Boolean),
        settings.notification_always.map(Choice::Boolean),
        settings.notify_on_completion.map(Choice::Boolean),
        settings.notify_on_pull_requests.map(Choice::Boolean),
        settings.notify_on_subtasks.map(Choice::Boolean),
        settings.notify_on_error.map(Choice::Boolean),
        settings.notify_on_question.map(Choice::Boolean),
        settings
            .notification_templates
            .clone()
            .map(Choice::Templates),
        settings.dictation_enabled.map(Choice::Boolean),
        settings.dictation_auto_send.map(Choice::Boolean),
        settings.stt_provider.map(Choice::Provider),
        settings.stt_server_url.clone().map(Choice::Text),
        settings.stt_model.clone().map(Choice::Text),
        settings.stt_endpoint_path.clone().map(Choice::Text),
        settings.stt_local_model.clone().map(Choice::Text),
        settings.stt_language.clone().map(Choice::Text),
        settings.stt_correct_transcript.map(Choice::Boolean),
        settings.worktree_archive_on_done.map(Choice::Boolean),
        settings.merged_worktree_cleanup.map(Choice::Boolean),
        order,
        hidden,
        explicit,
    ]
}

fn work_status_fields(settings: &ServerSettings) -> [Option<Choice>; 3] {
    [
        settings
            .work_status_section_order
            .as_ref()
            .and_then(WorkStatusSectionList::entries)
            .map(|order| Choice::Sections(work_status_sections::sanitize_order(order))),
        settings
            .work_status_hidden_sections
            .as_ref()
            .and_then(WorkStatusSectionList::entries)
            .map(|hidden| {
                Choice::Sections(work_status_sections::sanitize_hidden(
                    hidden,
                    settings.work_status_hidden_sections_explicit == Some(true),
                ))
            }),
        settings
            .work_status_hidden_sections_explicit
            .map(Choice::Boolean),
    ]
}

fn mode_setting(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => "ask",
        PermissionMode::Safety => "safety",
        PermissionMode::Auto => "auto",
    }
}

impl Write {
    fn changes(&self) -> SettingsChanges {
        let boolean = |field| match self.values[field] {
            Some(Choice::Boolean(value)) => Some(value),
            _ => None,
        };
        let text = |field| match &self.values[field] {
            Some(Choice::Text(text)) => Some(text.clone()),
            _ => None,
        };
        let sections = |field| match &self.values[field] {
            Some(Choice::Sections(sections)) => Some(sections.clone()),
            _ => None,
        };
        SettingsChanges {
            native_notifications_enabled: boolean(NOTIFY_ENABLED),
            notification_mode: boolean(NOTIFY_ALWAYS)
                .map(|always| if always { "always" } else { "hidden-only" }),
            notify_on_completion: boolean(NOTIFY_COMPLETION),
            notify_on_pull_requests: boolean(NOTIFY_PULL_REQUESTS),
            notify_on_subtasks: boolean(NOTIFY_SUBTASKS),
            notify_on_error: boolean(NOTIFY_ERROR),
            notify_on_question: boolean(NOTIFY_QUESTION),
            notification_templates: match &self.values[NOTIFY_TEMPLATES] {
                Some(Choice::Templates(templates)) => Some(templates.clone()),
                _ => None,
            },
            dictation_enabled: boolean(DICTATION_ENABLED),
            dictation_auto_send: boolean(DICTATION_AUTO_SEND),
            stt_provider: match self.values[STT_PROVIDER] {
                Some(Choice::Provider(provider)) => Some(provider.as_setting()),
                _ => None,
            },
            stt_server_url: text(STT_SERVER_URL),
            stt_model: text(STT_MODEL),
            stt_endpoint_path: text(STT_ENDPOINT_PATH),
            stt_local_model: text(STT_LOCAL_MODEL),
            stt_language: text(STT_LANGUAGE),
            stt_correct_transcript: boolean(STT_CORRECT),
            auto_save_enabled: boolean(AUTO_SAVE),
            terminal_close_on_exit: boolean(CLOSE_ON_EXIT),
            terminal_shell: match &self.values[SHELL] {
                Some(Choice::Shell(shell)) => Some(shell.clone()),
                _ => None,
            },
            prompt_navigator_enabled: boolean(PROMPT_NAVIGATOR),
            follow_up_behavior: match self.values[FOLLOW_UP] {
                Some(Choice::FollowUp(behavior)) => Some(behavior.as_setting()),
                _ => None,
            },
            draft_starters_visible: boolean(DRAFT_STARTERS),
            permission_default_mode: match self.values[PERMISSION_DEFAULT] {
                Some(Choice::Mode(mode)) => Some(mode_setting(mode)),
                _ => None,
            },
            sidebar_show_chats_section: boolean(SHOW_CHATS),
            session_work_enabled: boolean(WORK_ENABLED),
            show_deletion_dialog: boolean(DELETION_DIALOG),
            allow_prompting_subagent_sessions: boolean(SUBAGENT_PROMPTS),
            worktree_archive_on_done_enabled: boolean(ARCHIVE_ON_DONE),
            merged_worktree_cleanup_enabled: boolean(MERGED_CLEANUP),
            work_status_section_order: sections(WORK_STATUS_ORDER),
            work_status_hidden_sections: sections(WORK_STATUS_HIDDEN),
            work_status_hidden_sections_explicit: boolean(WORK_STATUS_EXPLICIT),
            ..SettingsChanges::default()
        }
    }
}

#[derive(Clone)]
struct Target {
    runtime: RuntimeId,
    generation: u64,
    client: ServerClient,
    network: tokio::runtime::Handle,
}

impl Target {
    fn matches(&self, other: &Self) -> bool {
        self.runtime == other.runtime && self.generation == other.generation
    }
}

#[derive(Default)]
struct RuntimeSettings {
    state: State,
    target: Option<Target>,
    read_revision: u64,
    read: Option<Task<()>>,
    write: Option<Task<()>>,
    pending_io: Option<NetworkTask<WriteResult>>,
    shells_read: Option<Task<()>>,
    shells: Vec<TerminalShell>,
    shells_loaded: bool,
    read_failed: bool,
    work_status_layout_admitted: bool,
    work_status_order_malformed: bool,
    work_status_hidden_malformed: bool,
    shells_failed: bool,
}

struct WorkStatusRead {
    clock: Rc<Cell<u64>>,
    ticket: u64,
    revisions: Option<[Option<u64>; 3]>,
}

impl WorkStatusRead {
    fn begin(clock: Rc<Cell<u64>>, revisions: Option<[Option<u64>; 3]>) -> Self {
        let ticket = clock.get() + 1;
        clock.set(ticket);
        Self {
            clock,
            ticket,
            revisions,
        }
    }

    fn is_current(&self, clock: &Rc<Cell<u64>>) -> bool {
        Rc::ptr_eq(&self.clock, clock) && self.ticket == clock.get()
    }
}

impl RuntimeSettings {
    fn preferences(&self, unsettled_only: bool) -> GeneralPreferences {
        let mut preferences = self.state.preferences(unsettled_only);
        for (list, malformed) in [
            (
                &mut preferences.work_status_section_order,
                self.work_status_order_malformed,
            ),
            (
                &mut preferences.work_status_hidden_sections,
                self.work_status_hidden_malformed,
            ),
        ] {
            if malformed && let Some(WorkStatusSectionList::Valid(entries)) = list.take() {
                *list = Some(WorkStatusSectionList::MalformedWithPrior(entries));
            }
        }
        preferences
    }

    fn work_status_read_revisions(&self) -> [Option<u64>; 3] {
        let revisions = self.state.read_revisions();
        WORK_STATUS_FIELDS.map(|field| revisions[field])
    }

    fn adopt_work_status(&mut self, read: &WorkStatusRead, settings: &ServerSettings) -> bool {
        if read.ticket != read.clock.get() {
            return false;
        }
        let Some(start_revisions) = read.revisions else {
            return false;
        };
        let mut revisions = [None; FIELDS];
        let mut values = std::array::from_fn(|_| None);
        let incoming_fields = WORK_STATUS_FIELDS
            .into_iter()
            .zip(start_revisions)
            .zip(work_status_fields(settings));
        for ((field, revision), value) in incoming_fields {
            revisions[field] = revision;
            values[field] = value;
        }
        self.state.adopt(revisions, values);
        self.work_status_order_malformed = settings
            .work_status_section_order
            .as_ref()
            .is_some_and(WorkStatusSectionList::is_malformed);
        self.work_status_hidden_malformed = settings
            .work_status_hidden_sections
            .as_ref()
            .is_some_and(WorkStatusSectionList::is_malformed);
        self.work_status_layout_admitted = true;
        true
    }
}

/// Authority captured before a Workspace settings request is dispatched.
/// It never retains a preference owner that the workspace has replaced.
pub(crate) struct WorkStatusSettingsRead {
    runtime: RuntimeId,
    generation: Option<u64>,
    owner: Option<WeakEntity<GeneralSettings>>,
    read: Option<WorkStatusRead>,
}

impl Workspace {
    pub(crate) fn begin_work_status_settings_read(
        &self,
        cx: &mut Context<Self>,
    ) -> WorkStatusSettingsRead {
        let general = self
            .settings
            .as_ref()
            .map(|settings| settings.read(cx).general.clone());
        let can_dispatch = self
            .connection
            .as_ref()
            .is_some_and(|connection| connection.client.is_some());
        let revisions = general.as_ref().and_then(|general| {
            general
                .read(cx)
                .runtimes
                .get(&self.runtime)
                .map(RuntimeSettings::work_status_read_revisions)
        });
        let read = can_dispatch.then(|| {
            WorkStatusRead::begin(
                Rc::clone(&self.send_catalog.work_status_read_clock),
                revisions,
            )
        });
        WorkStatusSettingsRead {
            runtime: self.runtime.clone(),
            generation: self
                .connection
                .as_ref()
                .map(|connection| connection.generation),
            owner: general.as_ref().map(Entity::downgrade),
            read,
        }
    }

    /// Consume the raw successful document before any fallback or optimistic
    /// overlay. Only the layout fields enter the retained settings owner.
    pub(crate) fn reconcile_work_status_settings(
        &self,
        read: &WorkStatusSettingsRead,
        incoming: &mut ServerSettings,
        cx: &mut Context<Self>,
    ) {
        let general = self
            .settings
            .as_ref()
            .map(|settings| settings.read(cx).general.clone());
        let same_scope = self.runtime == read.runtime
            && self
                .connection
                .as_ref()
                .map(|connection| connection.generation)
                == read.generation
            && read
                .read
                .as_ref()
                .is_some_and(|token| token.is_current(&self.send_catalog.work_status_read_clock));
        let same_owner = match (&read.owner, &general) {
            (None, None) => true,
            (Some(captured), Some(current)) => captured
                .upgrade()
                .is_some_and(|captured| captured.entity_id() == current.entity_id()),
            _ => false,
        };
        let admitted = if same_scope && same_owner {
            match (&general, read.read.as_ref()) {
                (Some(general), Some(token)) => general.update(cx, |general, cx| {
                    let adopted = general
                        .runtimes
                        .get_mut(&read.runtime)
                        .is_some_and(|slot| slot.adopt_work_status(token, incoming));
                    if adopted {
                        cx.notify();
                    }
                    adopted
                }),
                (None, Some(_)) => true,
                _ => false,
            }
        } else {
            false
        };
        if admitted {
            work_status_sections::retain_validated_layout(
                incoming,
                self.send_catalog.settings.as_ref(),
            );
        } else {
            // An older read or a different owner has no layout publication
            // authority, including no authority to change malformed flags.
            let previous = self.send_catalog.settings.as_ref();
            incoming.work_status_section_order =
                previous.and_then(|settings| settings.work_status_section_order.clone());
            incoming.work_status_hidden_sections =
                previous.and_then(|settings| settings.work_status_hidden_sections.clone());
            incoming.work_status_hidden_sections_explicit =
                previous.and_then(|settings| settings.work_status_hidden_sections_explicit);
        }
        if let Some(general) = general {
            general
                .read(cx)
                .overlay_work_status_settings(&self.runtime, incoming);
        }
    }
}

/// Known Settings choices. Missing fields leave the consumer's own state intact.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GeneralPreferences {
    pub(crate) work_status_section_order: Option<WorkStatusSectionList>,
    pub(crate) work_status_hidden_sections: Option<WorkStatusSectionList>,
    pub(crate) work_status_hidden_sections_explicit: Option<bool>,
    pub(crate) auto_save_enabled: Option<bool>,
    pub(crate) terminal_close_on_exit: Option<bool>,
    pub(crate) terminal_shell: Option<String>,
    pub(crate) prompt_navigator_enabled: Option<bool>,
    pub(crate) follow_up_behavior: Option<FollowUpBehavior>,
    pub(crate) draft_starters_visible: Option<bool>,
    pub(crate) permission_default_mode: Option<PermissionMode>,
    pub(crate) show_chats: Option<bool>,
    pub(crate) work_enabled: Option<bool>,
    pub(crate) show_deletion_dialog: Option<bool>,
    pub(crate) allow_prompting_subagent_sessions: Option<bool>,
    pub(crate) dictation_enabled: Option<bool>,
    pub(crate) dictation_auto_send: Option<bool>,
    pub(crate) stt_provider: Option<SttProvider>,
    pub(crate) stt_server_url: Option<String>,
    pub(crate) stt_model: Option<String>,
    pub(crate) stt_endpoint_path: Option<String>,
    pub(crate) stt_local_model: Option<String>,
    pub(crate) stt_language: Option<String>,
    pub(crate) stt_correct_transcript: Option<bool>,
    pub(crate) worktree_archive_on_done: Option<bool>,
    pub(crate) merged_worktree_cleanup: Option<bool>,
}

impl GeneralPreferences {
    /// Writes each known choice into the workspace's copy of the server settings.
    pub(crate) fn apply_to(&self, settings: &mut ServerSettings) {
        fn set<T: Clone>(target: &mut Option<T>, value: &Option<T>) {
            if value.is_some() {
                target.clone_from(value);
            }
        }
        set(
            &mut settings.work_status_section_order,
            &self.work_status_section_order,
        );
        set(
            &mut settings.work_status_hidden_sections,
            &self.work_status_hidden_sections,
        );
        set(
            &mut settings.work_status_hidden_sections_explicit,
            &self.work_status_hidden_sections_explicit,
        );
        set(&mut settings.auto_save_enabled, &self.auto_save_enabled);
        set(
            &mut settings.terminal_close_on_exit,
            &self.terminal_close_on_exit,
        );
        set(&mut settings.terminal_shell, &self.terminal_shell);
        set(
            &mut settings.prompt_navigator_enabled,
            &self.prompt_navigator_enabled,
        );
        set(&mut settings.follow_up_behavior, &self.follow_up_behavior);
        set(
            &mut settings.draft_starters_visible,
            &self.draft_starters_visible,
        );
        set(
            &mut settings.permission_default_mode,
            &self.permission_default_mode,
        );
        set(&mut settings.show_chats, &self.show_chats);
        set(&mut settings.work_enabled, &self.work_enabled);
        set(
            &mut settings.show_deletion_dialog,
            &self.show_deletion_dialog,
        );
        set(
            &mut settings.allow_prompting_subagent_sessions,
            &self.allow_prompting_subagent_sessions,
        );
        set(&mut settings.dictation_enabled, &self.dictation_enabled);
        set(&mut settings.dictation_auto_send, &self.dictation_auto_send);
        set(&mut settings.stt_provider, &self.stt_provider);
        set(&mut settings.stt_server_url, &self.stt_server_url);
        set(&mut settings.stt_model, &self.stt_model);
        set(&mut settings.stt_endpoint_path, &self.stt_endpoint_path);
        set(&mut settings.stt_local_model, &self.stt_local_model);
        set(&mut settings.stt_language, &self.stt_language);
        set(
            &mut settings.stt_correct_transcript,
            &self.stt_correct_transcript,
        );
        set(
            &mut settings.worktree_archive_on_done,
            &self.worktree_archive_on_done,
        );
        set(
            &mut settings.merged_worktree_cleanup,
            &self.merged_worktree_cleanup,
        );
    }
}

pub(crate) struct GeneralSettings {
    owner: WeakEntity<Workspace>,
    // The page outlives a host switch, including gaps between queued requests.
    network: Option<Network>,
    current: Option<Target>,
    runtimes: HashMap<RuntimeId, RuntimeSettings>,
    shell_picker: Entity<SelectState<SearchableVec<ShellOption>>>,
    picker_values: Vec<ShellOption>,
    scroll: ScrollHandle,
    anchors: [ScrollAnchor; 3],
    focus: [FocusHandle; 2],
    flush_waiters: Vec<tokio::sync::oneshot::Sender<()>>,
    closing: bool,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, PartialEq, Eq)]
struct ShellOption {
    value: String,
    label: SharedString,
}

impl SearchableListItem for ShellOption {
    type Value = String;
    fn title(&self) -> SharedString {
        self.label.clone()
    }
    fn value(&self) -> &String {
        &self.value
    }
}

impl GeneralSettings {
    pub(crate) fn new(
        owner: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let shell_picker = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(vec![ShellOption {
                    value: "auto".into(),
                    label: locale::strings(cx)
                        .get("settings.openchamber.visual.option.terminalShell.auto"),
                }]),
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        let subscription = cx.subscribe_in(&shell_picker, window, |this, _, event, _, cx| {
            if let SelectEvent::Confirm(Some(shell)) = event {
                this.change(SHELL, Choice::Shell(shell.to_string()), cx);
            }
        });
        let mut subscriptions = vec![subscription];
        if let Some(workspace) = owner.upgrade() {
            subscriptions.push(cx.observe(&workspace, |this, _, cx| {
                let target = this.capture(cx);
                if !same_target(this.current.as_ref(), target.as_ref()) {
                    this.refresh(cx);
                }
            }));
        }
        let weak = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = weak.update(cx, |this, cx| this.refresh(cx));
        });
        let scroll = ScrollHandle::new();
        let anchors = std::array::from_fn(|_| ScrollAnchor::for_handle(scroll.clone()));
        Self {
            owner,
            network: Network::new().ok(),
            current: None,
            runtimes: HashMap::new(),
            shell_picker,
            picker_values: Vec::new(),
            scroll,
            anchors,
            focus: std::array::from_fn(|_| cx.focus_handle().tab_index(0).tab_stop(true)),
            flush_waiters: Vec::new(),
            closing: false,
            _subscriptions: subscriptions,
        }
    }

    fn capture(&self, cx: &Context<Self>) -> Option<Target> {
        let owner = self.owner.upgrade()?;
        let workspace = owner.read(cx);
        let connection = workspace.connection.as_ref()?;
        Some(Target {
            runtime: workspace.runtime.clone(),
            generation: connection.generation,
            client: connection.client.clone()?,
            network: self.network.as_ref()?.handle(),
        })
    }

    /// Search targets are the canonical registry IDs; focus follows mounting.
    pub(crate) fn reveal(&mut self, item: &str, window: &mut Window, cx: &mut Context<Self>) {
        let field = match item {
            "appearance.auto-save-enabled" => AUTO_SAVE,
            "appearance.terminal-close-on-exit" => CLOSE_ON_EXIT,
            "appearance.terminal-shell" => SHELL,
            _ => return,
        };
        cx.on_next_frame(window, move |this, window, cx| {
            this.anchors[field].scroll_to(window, cx);
            if field == SHELL {
                this.shell_picker
                    .update(cx, |picker, cx| picker.focus(window, cx));
            } else {
                window.focus(&this.focus[field], cx);
            }
        });
    }

    /// Captures the active client before starting independent settings/shell reads.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.capture(cx) else {
            self.current = None;
            cx.notify();
            return;
        };
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let clock = Rc::clone(&owner.read(cx).send_catalog.work_status_read_clock);
        self.current = Some(target.clone());
        let slot = self.runtimes.entry(target.runtime.clone()).or_default();
        slot.target = Some(target.clone());
        slot.read_revision += 1;
        let read_revision = slot.read_revision;
        let revisions = slot.state.read_revisions();
        let work_status_read =
            WorkStatusRead::begin(clock, Some(slot.work_status_read_revisions()));
        let client = target.client.clone();
        let reading =
            NetworkTask::spawn_on(&target.network, async move { client.settings().await });
        let runtime = target.runtime.clone();
        let generation = target.generation;
        slot.read = Some(cx.spawn(async move |this, cx| {
            let result = reading.await;
            let _ = this.update(cx, |this, cx| {
                let layout_scope_current = this.owner.upgrade().is_some_and(|owner| {
                    let workspace = owner.read(cx);
                    workspace.runtime == runtime
                        && workspace
                            .connection
                            .as_ref()
                            .is_some_and(|connection| connection.generation == generation)
                        && work_status_read
                            .is_current(&workspace.send_catalog.work_status_read_clock)
                });
                let Some(slot) = this.runtimes.get_mut(&runtime) else {
                    return;
                };
                if slot.read_revision != read_revision {
                    return;
                }
                slot.read = None;
                match result {
                    Some(Ok(settings)) => {
                        let mut values = fields(&settings);
                        for field in WORK_STATUS_FIELDS {
                            values[field] = None;
                        }
                        slot.state.adopt(revisions, values);
                        if layout_scope_current {
                            slot.adopt_work_status(&work_status_read, &settings);
                        }
                        slot.read_failed = false;
                        this.apply(&runtime, cx);
                    }
                    _ => slot.read_failed = true,
                }
                cx.notify();
            });
        }));
        let api = TerminalApi::new(target.client.clone());
        let reading = NetworkTask::spawn_on(&target.network, async move { api.shells().await });
        let runtime = target.runtime;
        slot.shells_read = Some(cx.spawn(async move |this, cx| {
            let result = reading.await;
            let _ = this.update(cx, |this, cx| {
                let Some(slot) = this.runtimes.get_mut(&runtime) else {
                    return;
                };
                if slot.read_revision != read_revision {
                    return;
                }
                slot.shells_read = None;
                match result {
                    Some(Ok(shells)) => {
                        slot.shells = shells;
                        slot.shells_failed = false;
                        slot.shells_loaded = true;
                    }
                    _ => slot.shells_failed = true,
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn change(&mut self, field: usize, value: Choice, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        let Some(target) = self.capture(cx) else {
            return;
        };
        if !same_target(self.current.as_ref(), Some(&target)) {
            self.refresh(cx);
            return;
        }
        let slot = self.runtimes.entry(target.runtime.clone()).or_default();
        if slot.state.values[field] == value && !slot.state.failed[field] {
            return;
        }
        slot.target = Some(target.clone());
        slot.state.change(field, value);
        self.apply(&target.runtime, cx);
        self.save(&target.runtime, cx);
        cx.notify();
    }

    /// One visibility edit accepts the hidden list and its migration marker
    /// together. Both travel through the existing captured-runtime writer.
    pub(crate) fn set_work_status_hidden(
        &mut self,
        runtime: &RuntimeId,
        hidden: &[String],
        cx: &mut Context<Self>,
    ) {
        self.change_work_status(
            runtime,
            &[
                (
                    WORK_STATUS_HIDDEN,
                    Choice::Sections(work_status_sections::sanitize_hidden(hidden, true)),
                ),
                (WORK_STATUS_EXPLICIT, Choice::Boolean(true)),
            ],
            cx,
        );
    }

    pub(crate) fn set_work_status_order(
        &mut self,
        runtime: &RuntimeId,
        order: &[String],
        cx: &mut Context<Self>,
    ) {
        self.change_work_status(
            runtime,
            &[(
                WORK_STATUS_ORDER,
                Choice::Sections(work_status_sections::sanitize_order(order)),
            )],
            cx,
        );
    }

    fn change_work_status(
        &mut self,
        runtime: &RuntimeId,
        fields: &[(usize, Choice)],
        cx: &mut Context<Self>,
    ) {
        if self.closing {
            return;
        }
        let Some(target) = self.capture(cx) else {
            return;
        };
        if &target.runtime != runtime {
            return;
        }
        if !same_target(self.current.as_ref(), Some(&target)) {
            self.refresh(cx);
            return;
        }
        let slot = self.runtimes.entry(runtime.clone()).or_default();
        // A successful whole-document read can still have a superseded layout
        // response. Only admitted layout data can authorize whole-list edits.
        if !slot.work_status_layout_admitted {
            return;
        }
        if fields.iter().any(|(field, _)| {
            (*field == WORK_STATUS_ORDER
                && slot.work_status_order_malformed
                && !slot.state.known[WORK_STATUS_ORDER])
                || (*field == WORK_STATUS_HIDDEN
                    && slot.work_status_hidden_malformed
                    && !slot.state.known[WORK_STATUS_HIDDEN])
        }) {
            return;
        }
        if fields
            .iter()
            .all(|(field, value)| slot.state.values[*field] == *value && !slot.state.failed[*field])
        {
            return;
        }
        slot.target = Some(target);
        for (field, value) in fields {
            slot.state.change(*field, value.clone());
        }
        self.apply(runtime, cx);
        self.save(runtime, cx);
        cx.notify();
    }

    /// A still-rendered dialog cannot submit an old runtime's layout on a
    /// newly selected server, even before its observer has rerendered it.
    pub(crate) fn work_status_editable(
        &self,
        runtime: &RuntimeId,
        generation: Option<u64>,
    ) -> bool {
        self.editable()
            && self
                .runtimes
                .get(runtime)
                .is_some_and(|slot| slot.work_status_layout_admitted)
            && self.current.as_ref().is_some_and(|target| {
                &target.runtime == runtime && Some(target.generation) == generation
            })
    }

    pub(crate) fn work_status_order_editable(
        &self,
        runtime: &RuntimeId,
        generation: Option<u64>,
    ) -> bool {
        self.work_status_editable(runtime, generation)
            && self.runtimes.get(runtime).is_some_and(|slot| {
                !slot.work_status_order_malformed || slot.state.known[WORK_STATUS_ORDER]
            })
    }

    pub(crate) fn work_status_visibility_editable(
        &self,
        runtime: &RuntimeId,
        generation: Option<u64>,
    ) -> bool {
        self.work_status_editable(runtime, generation)
            && self.runtimes.get(runtime).is_some_and(|slot| {
                !slot.work_status_hidden_malformed || slot.state.known[WORK_STATUS_HIDDEN]
            })
    }

    /// Reads only this layout's fields; streamed workspace renders do not clone
    /// unrelated settings or their provider/project collections.
    pub(crate) fn work_status_layout(
        &self,
        runtime: &RuntimeId,
        fallback: Option<&ServerSettings>,
    ) -> work_status_sections::Layout {
        let state = self.runtimes.get(runtime).map(|slot| &slot.state);
        let sections = |field| {
            state
                .filter(|state| state.known[field])
                .map(|state| state.sections(field))
        };
        work_status_sections::Layout::from_settings(fallback)
            .with_known_lists(sections(WORK_STATUS_ORDER), sections(WORK_STATUS_HIDDEN))
    }

    fn overlay_work_status_settings(&self, runtime: &RuntimeId, settings: &mut ServerSettings) {
        let Some(slot) = self.runtimes.get(runtime) else {
            return;
        };
        let sections = |field: usize, malformed: bool| {
            slot.state.known[field].then(|| {
                let entries = slot.state.sections(field).to_vec();
                if malformed {
                    WorkStatusSectionList::MalformedWithPrior(entries)
                } else {
                    WorkStatusSectionList::Valid(entries)
                }
            })
        };
        GeneralPreferences {
            work_status_section_order: sections(
                WORK_STATUS_ORDER,
                slot.work_status_order_malformed,
            ),
            work_status_hidden_sections: sections(
                WORK_STATUS_HIDDEN,
                slot.work_status_hidden_malformed,
            ),
            work_status_hidden_sections_explicit: slot.state.known[WORK_STATUS_EXPLICIT]
                .then(|| slot.state.boolean(WORK_STATUS_EXPLICIT)),
            ..GeneralPreferences::default()
        }
        .apply_to(settings);
    }

    fn save(&mut self, runtime: &RuntimeId, cx: &mut Context<Self>) {
        let Some(slot) = self.runtimes.get_mut(runtime) else {
            return;
        };
        let Some(target) = slot.target.clone() else {
            return;
        };
        let Some(write) = slot.state.begin() else {
            return;
        };
        let changes = write.changes();
        let client = target.client;
        let (completed, receipt) = tokio::sync::oneshot::channel();
        // Retain the actual request independently of its foreground completion.
        slot.pending_io = Some(NetworkTask::spawn_on(&target.network, async move {
            let result = client
                .save_settings(&changes)
                .await
                .ok()
                .map(|settings| fields(&settings));
            let _ = completed.send(result.clone());
            result
        }));
        let runtime = runtime.clone();
        slot.write = Some(cx.spawn(async move |this, cx| {
            let result = receipt.await.ok().flatten();
            let _ = this.update(cx, |this, cx| {
                let Some(slot) = this.runtimes.get_mut(&runtime) else {
                    return;
                };
                slot.write = None;
                slot.pending_io = None;
                slot.state.finish(&write, result);
                this.save(&runtime, cx);
                if this.is_flushed() {
                    for waiting in this.flush_waiters.drain(..) {
                        let _ = waiting.send(());
                    }
                }
                cx.notify();
            });
        }));
    }

    fn apply(&self, runtime: &RuntimeId, cx: &mut Context<Self>) {
        let Some(preferences) = self.preferences(runtime) else {
            return;
        };
        let runtime = runtime.clone();
        let owner = self.owner.clone();
        let layout_owner = cx.weak_entity();
        // Defer avoids reading this entity while its Workspace observer runs.
        cx.defer(move |cx| {
            let _ = owner.update(cx, |owner, cx| {
                owner.apply_general_settings(&runtime, &preferences, cx);
                // A topology read can settle newer layout fields before this
                // deferred publication. Re-read only the three layout choices.
                if owner.runtime == runtime
                    && let Some(layout_owner) = layout_owner.upgrade()
                    && let Some(settings) = owner.send_catalog.settings.as_mut()
                {
                    layout_owner
                        .read(cx)
                        .overlay_work_status_settings(&runtime, settings);
                }
            });
        });
    }

    /// A failed choice remains the source for newly created native panels.
    pub(crate) fn preferences(&self, runtime: &RuntimeId) -> Option<GeneralPreferences> {
        self.runtimes
            .get(runtime)
            .map(|slot| slot.preferences(false))
    }

    /// Choices the server has not confirmed, which a newer server read must not replace.
    pub(crate) fn unsettled_preferences(&self, runtime: &RuntimeId) -> Option<GeneralPreferences> {
        self.runtimes
            .get(runtime)
            .map(|slot| slot.preferences(true))
    }

    /// Writes dispatch immediately; normal close can await this without retrying failures.
    pub(crate) fn is_flushed(&self) -> bool {
        self.runtimes
            .values()
            .all(|slot| slot.write.is_none() && !slot.state.pending.iter().any(|p| *p))
    }

    /// Normal-close barrier. Page dismissal leaves writes running in this entity.
    pub(crate) fn take_pending_write(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        self.closing = true;
        cx.notify();
        if self.is_flushed() {
            return None;
        }
        let (ready, waiting) = tokio::sync::oneshot::channel();
        self.flush_waiters.push(ready);
        Some(waiting)
    }

    /// Final app-quit drain: transfers accepted writes and their network runtime.
    /// Quit blocks foreground completions, so this task never updates the entity.
    /// Cancelable window close continues to use `take_pending_write`.
    pub(crate) fn take_quit_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.closing = true;
        self.flush_waiters.clear();
        let mut drains = Vec::new();
        for slot in self.runtimes.values_mut() {
            slot.write = None;
            let pending = slot.pending_io.take();
            slot.state.writing = None;
            let queued = slot.state.take_pending();
            if pending.is_none() && queued.is_none() {
                continue;
            }
            let target = slot.target.clone();
            drains.push(cx.background_executor().spawn(async move {
                if let Some(pending) = pending {
                    let _ = pending.await;
                }
                // Only newer edits follow the captured request. Failed fields
                // are absent from this snapshot until the user explicitly retries.
                if let (Some(write), Some(target)) = (queued, target) {
                    let changes = write.changes();
                    let client = target.client;
                    let saving = NetworkTask::spawn_on(&target.network, async move {
                        client.save_settings(&changes).await
                    });
                    let _ = saving.await;
                }
            }));
        }
        if drains.is_empty() {
            return None;
        }
        // A Tokio Handle cannot keep its Runtime alive after the page is dropped.
        // Each runtime's drain starts independently; one failure cannot block siblings.
        let network = self.network.take();
        Some(cx.background_executor().spawn(async move {
            for drain in drains {
                drain.await;
            }
            drop(network);
        }))
    }

    /// Reopens editing after a canceled close or a failed close-time write.
    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.closing = false;
        cx.notify();
    }

    pub(crate) fn has_failed_writes(&self) -> bool {
        self.runtimes
            .values()
            .any(|slot| slot.state.failed.iter().any(|failed| *failed))
    }

    fn current_slot(&self) -> Option<&RuntimeSettings> {
        self.runtimes.get(&self.current.as_ref()?.runtime)
    }

    /// The current server's choice, or the consumer default before it is known.
    pub(crate) fn current_choice(&self, field: usize) -> Choice {
        self.current_slot().map_or_else(
            || State::default().values[field].clone(),
            |slot| slot.state.values[field].clone(),
        )
    }

    /// Edits need a captured server and an open page.
    pub(crate) fn editable(&self) -> bool {
        self.current_slot().is_some() && !self.closing
    }

    /// The shared save indicator: its message and whether Retry is offered.
    /// `shells` includes the General page's shell list read.
    pub(crate) fn status(&self, shells: bool) -> (Option<&'static str>, bool) {
        let slot = self.current_slot();
        let failed = slot.is_some_and(|slot| slot.state.failed.iter().any(|failed| *failed));
        let read_failed = slot.is_some_and(|slot| {
            slot.read_failed
                // A superseded first document leaves no layout authority.
                // Keep the existing Retry reachable after that read finishes.
                || (!slot.work_status_layout_admitted && slot.read.is_none())
                || slot.work_status_order_malformed
                || slot.work_status_hidden_malformed
                || (shells && slot.shells_failed)
        });
        let key = if failed {
            Some("settings.common.status.saveFailed")
        } else if read_failed {
            Some("settings.openchamber.desktopNetwork.error.loadFailed")
        } else if slot.is_some_and(|slot| slot.write.is_some()) {
            Some("settings.common.actions.saving")
        } else if self.current.is_none() {
            Some("common.unavailable")
        } else {
            None
        };
        (key, failed || read_failed)
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing
    }

    pub(crate) fn retry(&mut self, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        let Some(target) = self.current.clone() else {
            return;
        };
        if let Some(slot) = self.runtimes.get_mut(&target.runtime) {
            slot.state.retry();
        }
        self.save(&target.runtime, cx);
        self.refresh(cx);
    }
}

fn same_target(a: Option<&Target>, b: Option<&Target>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.matches(b),
        (None, None) => true,
        _ => false,
    }
}

impl Render for GeneralSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let slot = self.current_slot();
        let enabled = slot.is_some() && !self.closing;
        let auto_save = slot.is_none_or(|slot| slot.state.boolean(AUTO_SAVE));
        let close_on_exit = slot.is_none_or(|slot| slot.state.boolean(CLOSE_ON_EXIT));
        let shell = slot.map_or("auto", |slot| slot.state.shell()).to_owned();
        let shell_enabled = !self.closing && slot.is_some_and(|slot| slot.shells_loaded);
        let mut shell_values = vec![ShellOption {
            value: "auto".into(),
            label: strings.get("settings.openchamber.visual.option.terminalShell.auto"),
        }];
        if let Some(slot) = slot {
            for candidate in &slot.shells {
                if candidate.id != "auto" {
                    shell_values.push(ShellOption {
                        value: candidate.id.clone(),
                        label: candidate.name.clone().into(),
                    });
                }
            }
        }
        if !shell_values.iter().any(|value| value.value == shell) {
            shell_values.push(ShellOption {
                value: shell.clone(),
                label: shell.clone().into(),
            });
        }
        if self.picker_values != shell_values
            || self.shell_picker.read(cx).selected_value() != Some(&shell)
        {
            self.picker_values = shell_values.clone();
            self.shell_picker.update(cx, |picker, cx| {
                picker.set_items(SearchableVec::new(shell_values), window, cx);
                picker.set_selected_value(&shell, window, cx);
            });
        }
        let palette = cx.global::<Palette>();
        let checkbox = |id: &'static str, key: &'static str, field: usize, checked: bool| {
            let weak = cx.weak_entity();
            gpui_kit::base::Checkbox::new(id)
                .accessibility_id(id)
                .accessibility_label(strings.get(key))
                .checked(checked)
                .disabled(!enabled)
                .track_focus(&self.focus[field])
                .anchor_scroll(Some(self.anchors[field].clone()))
                .flex_row_from(direction)
                .items_start()
                .gap(px(8.0))
                .py(px(5.0))
                .on_change(move |state, _, _, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.change(
                            field,
                            Choice::Boolean(state == gpui_kit::base::CheckboxState::Checked),
                            cx,
                        )
                    });
                })
                .child(glyph(
                    if checked {
                        "checkbox"
                    } else {
                        "checkbox-blank"
                    },
                    16.0,
                    palette.foreground,
                ))
                .child(div().min_w_0().flex_1().child(strings.get(key)))
        };
        let heading = |key: &'static str| {
            div()
                .id(key)
                .role(Role::Heading)
                .font_weight(FontWeight::SEMIBOLD)
                .mb(px(12.0))
                .child(strings.get(key))
        };
        let (status_key, retryable) = self.status(true);
        let retry = on_activate(
            gpui_kit::base::Button::new("general-retry")
                .disabled(self.closing)
                .accessibility_id("general-retry")
                .accessibility_label(strings.get("desktopHostSwitcher.actions.retry"))
                .h(gpui_kit::rems(1.75))
                .px(px(8.0))
                .rounded(px(7.0))
                .child(strings.get("desktopHostSwitcher.actions.retry")),
            cx,
            |this, _, cx| this.retry(cx),
        );
        div()
            .id("general-settings-page")
            .role(Role::Group)
            .aria_label(strings.get("settings.page.general.title"))
            .size_full()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(48.0))
            .py(px(32.0))
            .css_text(0.875, 1.25, cx)
            .text_color(palette.foreground)
            .when(direction == locale::Direction::RightToLeft, |view| {
                view.text_right()
            })
            .child(crate::settings_view::page_header(
                crate::settings_navigation::Page::General,
                cx,
            ))
            .child(heading("settings.layout.general.files.title"))
            .child(checkbox(
                "general-auto-save",
                "settings.openchamber.visual.field.autoSaveEnabled",
                AUTO_SAVE,
                auto_save,
            ))
            .child(
                div()
                    .mt(px(32.0))
                    .pt(px(24.0))
                    .border_t_1()
                    .border_color(palette.border)
                    .child(heading("settings.layout.general.terminal.title"))
                    .child(
                        div()
                            .id("general-terminal-shell-row")
                            .key_context("SettingsPicker")
                            .anchor_scroll(Some(self.anchors[SHELL].clone()))
                            .flex_row_from(direction)
                            .flex_wrap()
                            .items_center()
                            .gap(px(12.0))
                            .mb(px(12.0))
                            .child(strings.get("settings.openchamber.visual.field.terminalShell"))
                            .child(
                                div()
                                    .w(px(240.0))
                                    .max_w_full()
                                    .h(gpui_kit::rems(2.0))
                                    .child(
                                    Select::new(&self.shell_picker)
                                        .id("general-terminal-shell")
                                        .accessibility_label(strings.get(
                                            "settings.openchamber.visual.field.terminalShellAria",
                                        ))
                                        .disabled(!shell_enabled)
                                        .w_full()
                                        .h(gpui_kit::rems(2.0)),
                                ),
                            ),
                    )
                    .child(checkbox(
                        "general-terminal-close-on-exit",
                        "settings.openchamber.visual.field.terminalCloseOnExit",
                        CLOSE_ON_EXIT,
                        close_on_exit,
                    )),
            )
            .child(
                div()
                    .id("general-save-status")
                    .role(Role::Status)
                    .h(px(36.0))
                    .mt(px(16.0))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .text_color(palette.muted_foreground)
                    .children(status_key.map(|key| strings.get(key)))
                    .when(retryable, |view| view.child(retry)),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::endpoint::Endpoint;
    use serde_json::{Value, json};

    use super::*;

    #[derive(Clone, Debug)]
    struct Request {
        target: String,
        authorization: Option<String>,
        changes: Value,
    }

    struct SettingsServer {
        endpoint: Endpoint,
        address: std::net::SocketAddr,
        requests: Arc<Mutex<Vec<Request>>>,
        saved: Arc<Mutex<serde_json::Map<String, Value>>>,
        arrived: mpsc::Receiver<()>,
        release_first: mpsc::Sender<()>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl SettingsServer {
        fn new(fail_first: bool, release_peer_on_second: Option<mpsc::Sender<()>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("owned loopback listener");
            let address = listener.local_addr().expect("listener address");
            let endpoint = Endpoint::parse(&format!("http://{address}")).expect("endpoint");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let saved = Arc::new(Mutex::new(serde_json::Map::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let recorded = Arc::clone(&requests);
            let persisted = Arc::clone(&saved);
            let stopped = Arc::clone(&stop);
            let (arrival, arrived) = mpsc::channel();
            let (release_first, released) = mpsc::channel();
            let worker = thread::spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    let (mut stream, _) = listener.accept().expect("accept loopback request");
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .expect("bounded request read");
                    let Some(request) = read_request(&mut stream) else {
                        continue;
                    };
                    let changes = request.changes.clone();
                    let index = {
                        let mut requests = recorded.lock().expect("requests");
                        requests.push(request);
                        requests.len()
                    };
                    let _ = arrival.send(());
                    if index == 1 {
                        released
                            .recv_timeout(Duration::from_secs(10))
                            .expect("test releases the first response");
                    }
                    let (status, body) = if fail_first && index == 1 {
                        (500, json!({"error": "fixture write failure"}))
                    } else {
                        let mut saved = persisted.lock().expect("saved settings");
                        saved.extend(changes.as_object().expect("settings object").clone());
                        (200, Value::Object(saved.clone()))
                    };
                    if index == 2
                        && let Some(peer) = &release_peer_on_second
                    {
                        let _ = peer.send(());
                    }
                    let body = body.to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            });
            Self {
                endpoint,
                address,
                requests,
                saved,
                arrived,
                release_first,
                stop,
                worker: Some(worker),
            }
        }

        fn wait_for_request(&self) {
            self.arrived
                .recv_timeout(Duration::from_secs(3))
                .expect("captured request reaches the server");
        }

        fn requests(&self) -> Vec<Request> {
            self.requests.lock().expect("requests").clone()
        }

        fn saved(&self) -> Value {
            Value::Object(self.saved.lock().expect("saved settings").clone())
        }
    }

    impl Drop for SettingsServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            let _ = self.release_first.send(());
            let _ = TcpStream::connect(self.address);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> Option<Request> {
        let mut headers = Vec::new();
        loop {
            let mut byte = [0];
            stream.read_exact(&mut byte).ok()?;
            headers.push(byte[0]);
            if headers.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let headers = String::from_utf8(headers).expect("HTTP headers");
        let mut first = headers
            .lines()
            .next()
            .expect("request line")
            .split_whitespace();
        assert_eq!(first.next(), Some("PUT"), "only accepted writes are sent");
        let target = first.next().expect("request target").to_owned();
        let header = |name: &str| {
            headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.trim().to_owned())
        };
        let length: usize = header("content-length")
            .expect("request length")
            .parse()
            .expect("numeric length");
        let mut body = vec![0; length];
        stream.read_exact(&mut body).expect("settings body");
        Some(Request {
            target,
            authorization: header("authorization"),
            changes: serde_json::from_slice(&body).expect("settings JSON"),
        })
    }

    fn open_page(cx: &mut gpui_kit::TestAppContext) -> Entity<GeneralSettings> {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            cx.new(|cx| GeneralSettings::new(workspace.downgrade(), window, cx))
        })
        .expect("test window")
    }

    fn set_target(
        page: &mut GeneralSettings,
        runtime: &RuntimeId,
        generation: u64,
        server: &SettingsServer,
        token: &str,
    ) {
        page.runtimes.entry(runtime.clone()).or_default().target = Some(Target {
            runtime: runtime.clone(),
            generation,
            client: ServerClient::new(
                server.endpoint.clone(),
                Credential::ClientToken(token.to_owned()),
            )
            .expect("fixture client"),
            network: page
                .network
                .as_ref()
                .expect("page's network runtime")
                .handle(),
        });
    }

    /// Uses the real shutdown callback while keeping integration glue out of this test.
    fn drain_on_quit(
        page: Entity<GeneralSettings>,
        release: Vec<mpsc::Sender<()>>,
        cx: &mut gpui_kit::TestAppContext,
    ) -> WeakEntity<GeneralSettings> {
        let weak = page.downgrade();
        cx.update(|cx| {
            cx.on_app_quit(move |cx| {
                let pending = page.update(cx, |page, cx| page.take_quit_write(cx));
                for gate in &release {
                    let _ = gate.send(());
                }
                async move {
                    if let Some(pending) = pending {
                        pending.await;
                    }
                }
            })
            .detach();
        });
        weak
    }

    enum QuitPath {
        Application,
        ClosingWindow,
    }

    fn quit_with_coalesced_writes(cx: &mut gpui_kit::TestAppContext, path: QuitPath) {
        let server = SettingsServer::new(false, None);
        let page = open_page(cx);
        let runtime = RuntimeId::from("quit-owner");
        page.update(cx, |page, cx| {
            set_target(page, &runtime, 1, &server, "oc_client_first_fixture");
            page.runtimes
                .get_mut(&runtime)
                .expect("runtime")
                .state
                .change(AUTO_SAVE, Choice::Boolean(false));
            page.save(&runtime, cx);
        });
        server.wait_for_request();
        let waiting = page.update(cx, |page, cx| {
            set_target(page, &runtime, 2, &server, "oc_client_new_fixture");
            let slot = page.runtimes.get_mut(&runtime).expect("runtime");
            slot.state.change(AUTO_SAVE, Choice::Boolean(true));
            slot.state.change(CLOSE_ON_EXIT, Choice::Boolean(false));
            slot.state.change(SHELL, Choice::Shell("zsh".into()));
            slot.state.change(SHELL, Choice::Shell("fish".into()));
            page.save(&runtime, cx);
            if matches!(path, QuitPath::ClosingWindow) {
                let waiting = page.take_pending_write(cx);
                assert!(waiting.is_some(), "normal close waits for accepted writes");
                assert!(
                    page.network.is_some(),
                    "normal close keeps the executor owner"
                );
                waiting
            } else {
                None
            }
        });
        let _normal_close = waiting.map(|waiting| {
            cx.foreground_executor().spawn(async move {
                let _ = waiting.await;
            })
        });
        assert_eq!(
            server.requests().len(),
            1,
            "the first response is still held"
        );
        let weak = drain_on_quit(page, vec![server.release_first.clone()], cx);
        cx.executor().allow_parking();
        cx.quit();
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "quit drains the newest accepted choice");
        assert_eq!(requests[0].changes, json!({"autoSaveEnabled": false}));
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer oc_client_first_fixture"),
            "the in-flight write retains its original client"
        );
        assert_eq!(
            requests[1].authorization.as_deref(),
            Some("Bearer oc_client_new_fixture"),
            "queued writes retain the client captured before quit"
        );
        assert!(
            requests
                .iter()
                .all(|request| { request.target == "/api/config/settings?surface=desktop" })
        );
        let newest = json!({
            "autoSaveEnabled": true,
            "terminalCloseOnExit": false,
            "terminalShell": "fish"
        });
        assert_eq!(requests[1].changes, newest, "intermediate choices coalesce");
        assert_eq!(
            server.saved(),
            newest,
            "durable immediately when quit returns"
        );
        assert!(
            weak.upgrade().is_none(),
            "the drain outlives the page entity"
        );
    }

    #[gpui_kit::test]
    fn app_quit_drains_captured_and_newest_coalesced_choices(cx: &mut gpui_kit::TestAppContext) {
        quit_with_coalesced_writes(cx, QuitPath::Application);
    }

    #[gpui_kit::test]
    fn app_quit_drains_writes_already_waited_by_normal_close(cx: &mut gpui_kit::TestAppContext) {
        quit_with_coalesced_writes(cx, QuitPath::ClosingWindow);
    }

    #[gpui_kit::test]
    fn app_quit_drains_independent_runtimes_without_replaying_a_failed_field(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let blocked = SettingsServer::new(true, None);
        // The slow runtime cannot finish until its sibling's queued choice arrives.
        let independent = SettingsServer::new(false, Some(blocked.release_first.clone()));
        let page = open_page(cx);
        let blocked_runtime = RuntimeId::from("blocked-quit-owner");
        let independent_runtime = RuntimeId::from("independent-quit-owner");
        page.update(cx, |page, cx| {
            set_target(
                page,
                &blocked_runtime,
                1,
                &blocked,
                "oc_client_blocked_fixture",
            );
            page.runtimes
                .get_mut(&blocked_runtime)
                .expect("blocked runtime")
                .state
                .change(SHELL, Choice::Shell("zsh".into()));
            page.save(&blocked_runtime, cx);
            set_target(
                page,
                &independent_runtime,
                2,
                &independent,
                "oc_client_independent_fixture",
            );
            page.runtimes
                .get_mut(&independent_runtime)
                .expect("independent runtime")
                .state
                .change(AUTO_SAVE, Choice::Boolean(false));
            page.save(&independent_runtime, cx);
        });
        blocked.wait_for_request();
        independent.wait_for_request();
        page.update(cx, |page, cx| {
            page.runtimes
                .get_mut(&blocked_runtime)
                .expect("blocked runtime")
                .state
                .change(CLOSE_ON_EXIT, Choice::Boolean(false));
            page.save(&blocked_runtime, cx);
            page.runtimes
                .get_mut(&independent_runtime)
                .expect("independent runtime")
                .state
                .change(SHELL, Choice::Shell("fish".into()));
            page.save(&independent_runtime, cx);
        });
        drain_on_quit(page, vec![independent.release_first.clone()], cx);
        cx.executor().allow_parking();
        cx.quit();
        let blocked_requests = blocked.requests();
        assert_eq!(
            blocked_requests.len(),
            2,
            "failed writes are never replayed"
        );
        assert_eq!(blocked_requests[0].changes, json!({"terminalShell": "zsh"}));
        assert_eq!(
            blocked_requests[1].changes,
            json!({"terminalCloseOnExit": false}),
            "an unrelated queued field can save after the failure"
        );
        assert_eq!(blocked.saved(), json!({"terminalCloseOnExit": false}));
        assert!(blocked_requests.iter().all(|request| {
            request.authorization.as_deref() == Some("Bearer oc_client_blocked_fixture")
        }));
        let independent_requests = independent.requests();
        assert_eq!(independent_requests.len(), 2, "every runtime is drained");
        assert!(independent_requests.iter().all(|request| {
            request.authorization.as_deref() == Some("Bearer oc_client_independent_fixture")
        }));
        assert_eq!(
            independent.saved(),
            json!({"autoSaveEnabled": false, "terminalShell": "fish"}),
            "the sibling dispatches its queued choice while the first runtime is blocked"
        );
    }

    #[gpui_kit::test]
    fn normal_close_preserves_failure_for_explicit_retry_and_quit_does_not_replay_it(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let server = SettingsServer::new(true, None);
        let page = open_page(cx);
        let runtime = RuntimeId::from("failed-quit-owner");
        let waiting = page.update(cx, |page, cx| {
            set_target(page, &runtime, 1, &server, "oc_client_failure_fixture");
            page.runtimes
                .get_mut(&runtime)
                .expect("runtime")
                .state
                .change(AUTO_SAVE, Choice::Boolean(false));
            page.save(&runtime, cx);
            page.take_pending_write(cx).expect("normal-close barrier")
        });
        server.wait_for_request();
        server
            .release_first
            .send(())
            .expect("release failed request");
        cx.executor().allow_parking();
        cx.foreground_executor().block_test(async move {
            waiting.await.expect("normal-close completion");
        });
        page.update(cx, |page, cx| {
            assert!(page.is_flushed(), "the failed request has settled");
            assert!(
                page.has_failed_writes(),
                "normal close still observes failure"
            );
            assert!(page.network.is_some(), "normal close retains the executor");
            page.cancel_close(cx);
            assert!(!page.closing, "a failed close restores editing");
        });
        drain_on_quit(page, Vec::new(), cx);
        cx.quit();
        assert_eq!(
            server.requests().len(),
            1,
            "quit never retries a failed field"
        );
        assert_eq!(
            server.saved(),
            json!({}),
            "the rejected choice is not persisted"
        );
    }

    /// A server read carrying only `entries`.
    fn read(entries: &[(usize, Choice)]) -> Values {
        let mut values: Values = std::array::from_fn(|_| None);
        for (field, choice) in entries {
            values[*field] = Some(choice.clone());
        }
        values
    }

    #[test]
    fn unsettled_preferences_hold_only_choices_the_server_has_not_confirmed() {
        let mut state = State::default();
        state.adopt(
            state.read_revisions(),
            read(&[(SHOW_CHATS, Choice::Boolean(false))]),
        );
        state.change(PROMPT_NAVIGATOR, Choice::Boolean(false));
        state.change(WORK_ENABLED, Choice::Boolean(false));
        let write = state.begin().expect("both edits dispatch together");
        assert_eq!(
            serde_json::to_value(write.changes()).expect("serializes"),
            json!({"promptNavigatorEnabled": false, "sessionWorkEnabled": false})
        );
        let mut response = read(&[(WORK_ENABLED, Choice::Boolean(false))]);
        response[SHOW_CHATS] = Some(Choice::Boolean(false));
        state.finish(&write, Some(response));
        assert!(state.failed[PROMPT_NAVIGATOR], "the server did not echo it");
        assert_eq!(
            state.preferences(true),
            GeneralPreferences {
                prompt_navigator_enabled: Some(false),
                ..GeneralPreferences::default()
            }
        );
        assert_eq!(state.preferences(false).show_chats, Some(false));
        assert_eq!(state.preferences(false).work_enabled, Some(false));

        state.change(FOLLOW_UP, Choice::FollowUp(FollowUpBehavior::Steer));
        state.change(PERMISSION_DEFAULT, Choice::Mode(PermissionMode::Safety));
        state.change(DRAFT_STARTERS, Choice::Boolean(false));
        state.change(DELETION_DIALOG, Choice::Boolean(false));
        state.change(SHOW_CHATS, Choice::Boolean(true));
        let write = state.begin().expect("the next edits dispatch");
        assert_eq!(
            serde_json::to_value(write.changes()).expect("serializes"),
            json!({
                "followUpBehavior": "steer",
                "permissionDefaultMode": "safety",
                "draftStartersVisible": false,
                "showDeletionDialog": false,
                "sidebarShowChatsSection": true,
            }),
            "a failed field waits for an explicit retry"
        );
        let mut settings = ServerSettings::default();
        state.preferences(true).apply_to(&mut settings);
        assert_eq!(settings.follow_up_behavior, Some(FollowUpBehavior::Steer));
        assert_eq!(
            settings.permission_default_mode,
            Some(PermissionMode::Safety)
        );
        assert_eq!(settings.show_chats, Some(true));
        assert_eq!(settings.prompt_navigator_enabled, Some(false));
        assert_eq!(
            settings.work_enabled, None,
            "confirmed choices are not overlaid"
        );
    }

    #[test]
    fn notification_and_voice_fields_write_their_server_names_and_reach_the_composer() {
        let mut state = State::default();
        assert!(!state.boolean(NOTIFY_ENABLED), "notifications start off");
        assert!(state.boolean(DICTATION_ENABLED), "dictation starts on");
        state.change(NOTIFY_ALWAYS, Choice::Boolean(true));
        state.change(
            STT_PROVIDER,
            Choice::Provider(SttProvider::OpenAiCompatible),
        );
        state.change(STT_LANGUAGE, Choice::Text("uk".into()));
        let mut templates = NotificationTemplates::default();
        templates.error.title = "Failed".into();
        state.change(NOTIFY_TEMPLATES, Choice::Templates(templates));
        let write = state.begin().expect("edits dispatch together");
        let changes = serde_json::to_value(write.changes()).expect("serializes");
        assert_eq!(changes["notificationMode"], json!("always"));
        assert_eq!(changes["sttProvider"], json!("openai-compatible"));
        assert_eq!(changes["sttLanguage"], json!("uk"));
        assert_eq!(
            changes["notificationTemplates"]["error"]["title"],
            json!("Failed")
        );
        assert_eq!(changes.as_object().map(|fields| fields.len()), Some(4));
        state.finish(&write, Some(write.values.clone()));
        let mut settings = ServerSettings::default();
        state.preferences(false).apply_to(&mut settings);
        assert_eq!(settings.stt_provider, Some(SttProvider::OpenAiCompatible));
        assert_eq!(settings.stt_language.as_deref(), Some("uk"));
        assert_eq!(
            settings.dictation_enabled, None,
            "unknown until read or chosen"
        );
        state.adopt(
            state.read_revisions(),
            read(&[(DICTATION_ENABLED, Choice::Boolean(false))]),
        );
        state.preferences(false).apply_to(&mut settings);
        assert_eq!(settings.dictation_enabled, Some(false));
    }

    #[test]
    fn partial_reads_only_establish_authority_for_present_valid_fields() {
        let mut state = State::default();
        assert_eq!(
            state.preferences(false),
            GeneralPreferences::default(),
            "display defaults have no authority over consumers"
        );
        state.adopt(
            state.read_revisions(),
            read(&[(AUTO_SAVE, Choice::Boolean(false))]),
        );
        assert_eq!(
            state.preferences(false),
            GeneralPreferences {
                auto_save_enabled: Some(false),
                ..GeneralPreferences::default()
            }
        );
        state.adopt(
            state.read_revisions(),
            read(&[(CLOSE_ON_EXIT, Choice::Boolean(false))]),
        );
        assert_eq!(
            state.preferences(false),
            GeneralPreferences {
                auto_save_enabled: Some(false),
                terminal_close_on_exit: Some(false),
                ..GeneralPreferences::default()
            },
            "a later omitted field preserves the earlier authoritative value"
        );
    }

    #[test]
    fn an_edit_establishes_only_its_own_field_even_when_the_save_fails() {
        let mut state = State::default();
        state.change(SHELL, Choice::Shell("fish".into()));
        let write = state.begin().expect("shell edit dispatches");
        state.finish(&write, None);
        state.adopt(state.read_revisions(), read(&[]));
        assert_eq!(
            state.preferences(false),
            GeneralPreferences {
                terminal_shell: Some("fish".into()),
                ..GeneralPreferences::default()
            }
        );
        assert!(
            state.failed[SHELL],
            "failed chosen value stays authoritative locally"
        );
    }

    #[gpui_kit::test]
    fn close_freezes_retry_and_edits_until_canceled(cx: &mut gpui_kit::TestAppContext) {
        use openchamber_native_transport::credential::Credential;
        use openchamber_native_transport::endpoint::Endpoint;

        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            let page = cx.new(|cx| GeneralSettings::new(workspace.downgrade(), window, cx));
            page.update(cx, |page, cx| {
                let runtime = RuntimeId::from("close-regression");
                // No request is dispatched: this slot deliberately has no target.
                page.current = Some(Target {
                    runtime: runtime.clone(),
                    generation: 1,
                    client: ServerClient::new(
                        Endpoint::parse("http://127.0.0.1:1").expect("endpoint"),
                        Credential::None,
                    )
                    .expect("client"),
                    network: page.network.as_ref().expect("network executor").handle(),
                });
                let slot = page.runtimes.entry(runtime.clone()).or_default();
                slot.state.change(AUTO_SAVE, Choice::Boolean(false));
                let write = slot.state.begin().expect("prepared edit");
                slot.state.finish(&write, None);
                let revision = slot.state.revisions;

                assert!(
                    page.take_pending_write(cx).is_none(),
                    "a failed settled write needs no drain"
                );
                assert!(page.closing, "close freezes even an already-drained page");
                assert!(
                    page.is_flushed(),
                    "failure is separate from flush completion"
                );
                assert!(page.has_failed_writes());
                page.retry(cx);
                page.change(AUTO_SAVE, Choice::Boolean(true), cx);
                let slot = &page.runtimes[&runtime];
                assert!(
                    slot.state.failed[AUTO_SAVE],
                    "closed retry must not clear the failed choice"
                );
                assert_eq!(
                    slot.state.revisions, revision,
                    "closed controls cannot create another write"
                );

                page.cancel_close(cx);
                assert!(!page.closing, "canceled or failed close restores editing");
                page.retry(cx);
                assert!(
                    page.runtimes[&runtime].state.pending[AUTO_SAVE],
                    "retry becomes usable after cancel"
                );
            });
        })
        .expect("test window");
    }

    #[test]
    fn serial_writes_keep_newer_choices_and_failed_fields_remain_retryable() {
        let mut state = State::default();
        state.change(AUTO_SAVE, Choice::Boolean(false));
        let first = state.begin().expect("first edit dispatches");
        state.change(AUTO_SAVE, Choice::Boolean(true));
        state.change(CLOSE_ON_EXIT, Choice::Boolean(false));
        assert!(state.begin().is_none(), "only one write per runtime");
        state.finish(&first, None);
        assert!(
            !state.failed[AUTO_SAVE],
            "failure of an older value does not fail a newer edit"
        );
        let second = state.begin().expect("newer edits follow");
        state.finish(&second, None);
        assert_eq!(
            state.failed,
            std::array::from_fn(|field| field == AUTO_SAVE || field == CLOSE_ON_EXIT)
        );
        state.change(SHELL, Choice::Shell("zsh".into()));
        let third = state.begin().expect("unrelated field still dispatches");
        assert!(third.values[AUTO_SAVE].is_none());
        state.finish(&third, Some(third.values.clone()));
        state.retry();
        let retry = state.begin().expect("failed choices can retry");
        assert_eq!(retry.values[AUTO_SAVE], Some(Choice::Boolean(true)));
        assert_eq!(retry.values[CLOSE_ON_EXIT], Some(Choice::Boolean(false)));
        assert!(retry.values[SHELL].is_none());
    }

    #[test]
    fn reads_cannot_replace_newer_pending_in_flight_or_failed_choices() {
        let mut state = State::default();
        let revisions = state.read_revisions();
        state.change(AUTO_SAVE, Choice::Boolean(false));
        let snapshot = read(&[
            (AUTO_SAVE, Choice::Boolean(true)),
            (CLOSE_ON_EXIT, Choice::Boolean(false)),
        ]);
        state.adopt(revisions, snapshot.clone());
        assert!(!state.boolean(AUTO_SAVE));
        assert!(
            !state.boolean(CLOSE_ON_EXIT),
            "unrelated authoritative fields are adopted"
        );
        let write = state.begin().expect("edit dispatches");
        state.adopt(state.read_revisions(), snapshot.clone());
        assert!(!state.boolean(AUTO_SAVE));
        state.finish(&write, None);
        state.adopt(state.read_revisions(), snapshot);
        assert!(
            !state.boolean(AUTO_SAVE),
            "failed choice stays visible until retry"
        );
    }

    #[test]
    fn successful_write_still_invalidates_a_read_started_before_the_edit() {
        let mut state = State::default();
        let revisions = state.read_revisions();
        state.change(AUTO_SAVE, Choice::Boolean(false));
        let write = state.begin().expect("edit dispatches");
        state.finish(&write, Some(write.values.clone()));
        state.adopt(revisions, read(&[(AUTO_SAVE, Choice::Boolean(true))]));
        assert!(!state.boolean(AUTO_SAVE));
        assert!(!state.failed[AUTO_SAVE]);
    }

    #[test]
    fn read_started_during_a_write_cannot_revert_it_after_the_write_finishes() {
        let mut state = State::default();
        state.change(AUTO_SAVE, Choice::Boolean(false));
        let write = state.begin().expect("edit dispatches");
        let during_write = state.read_revisions();
        state.finish(&write, Some(write.values.clone()));
        state.adopt(during_write, read(&[(AUTO_SAVE, Choice::Boolean(true))]));
        assert!(
            !state.boolean(AUTO_SAVE),
            "the read may have observed the old server value"
        );
    }

    #[test]
    fn a_runtime_switch_leaves_each_owners_queue_and_failure_independent() {
        let mut old = State::default();
        let mut new = State::default();
        old.change(SHELL, Choice::Shell("zsh".into()));
        let old_write = old.begin().expect("old runtime dispatches");
        old.change(CLOSE_ON_EXIT, Choice::Boolean(false));
        new.change(AUTO_SAVE, Choice::Boolean(false));
        let new_write = new
            .begin()
            .expect("new runtime does not wait for old runtime");
        old.finish(&old_write, None);
        let queued = old.begin().expect("old queue still belongs to old runtime");
        assert!(queued.values[AUTO_SAVE].is_none());
        assert_eq!(queued.values[CLOSE_ON_EXIT], Some(Choice::Boolean(false)));
        new.finish(&new_write, Some(new_write.values.clone()));
        assert_eq!(new.failed, [false; FIELDS]);
        assert!(old.failed[SHELL]);
        assert_eq!(new.shell(), "auto");
    }
}
