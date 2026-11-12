//! Settings → MCP (`McpGrid.tsx`, `McpPage.tsx`): the MCP servers the current
//! directory's config files define, and one server's editor.
//!
//! An existing server autosaves: the on/off box, the connection kind and the
//! pickers write at once, text writes when it loses focus, and a write goes
//! out only when the form differs from the last one saved. A new server
//! exists only once Create confirms it. Every changed form is its own
//! accepted write in `SettingsWrites`, which sends a runtime's writes in
//! acceptance order; a server with a delete queued accepts no save.
//!
//! Connection status has one owner, the workspace's `McpStatusStore`, which
//! the work status card also shows. This page reads a directory's servers
//! with their reasons and hands the statuses to that store, so a change made
//! here reaches the card's rows. The page keeps only the reason text.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui_kit::component::IndexPath;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Theme, WindowExt};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, WeakEntity, Window,
    div, prelude::FluentBuilder, px,
};
use openchamber_native_transport::Url;
use openchamber_native_transport::mcp::McpConnection;
use openchamber_native_transport::mcp_config::{
    IntegrationPolicy, McpConfigError, McpDraft, McpKind, McpOAuthIntegration, McpOAuthSetting,
    McpProtocol, McpScope, McpServerConfig, McpServerReport, OAuthAttempt, OAuthMode,
    OAuthProgress, is_integration_denied,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::mcp_status::RefreshTicket;
use crate::network::{Network, NetworkTask};
use crate::settings_choice::{ChipOption, chip_group, icon_button, pill, text_button};
use crate::settings_controls::{
    Load, ServerTarget, SettingsWrites, checkbox_row, confirm, field_row, focus_handle, helper,
    load_failed, page_frame, same_target,
};
use crate::settings_navigation::Page;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// How long a missing OAuth integration is looked for after a server opens:
/// OpenCode registers it on the server's first attempt.
const INTEGRATION_LOOKUPS: usize = 9;
const INTEGRATION_LOOKUP_INTERVAL: Duration = Duration::from_millis(750);
/// How often a browser sign-in is checked (`STATUS_POLL_INTERVAL_MS`).
const SIGN_IN_POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// Everything the page writes into a server's entry and compares to decide
/// whether a save is due. A row without a key writes nothing, so it does not
/// count as a change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Form {
    kind: McpKind,
    command: Vec<String>,
    url: String,
    environment: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    timeout_startup: String,
    timeout_catalog: String,
    timeout_execution: String,
    codemode: Option<bool>,
    protocol: McpProtocol,
    /// Empty for a local server.
    metadata_url: String,
    enabled: bool,
}

/// The OAuth credentials the page has no editor for. A save rebuilds the
/// whole `oauth` block, so they are carried and written back untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Carried {
    enabled: bool,
    client_id: String,
    client_secret: String,
    scope: String,
    redirect_uri: String,
    callback_port: String,
}

impl Default for Carried {
    fn default() -> Self {
        Self {
            enabled: true,
            client_id: String::new(),
            client_secret: String::new(),
            scope: String::new(),
            redirect_uri: String::new(),
            callback_port: String::new(),
        }
    }
}

impl Carried {
    fn of(oauth: &McpOAuthSetting) -> Self {
        match oauth {
            McpOAuthSetting::Unset => Self::default(),
            McpOAuthSetting::Disabled => Self {
                enabled: false,
                ..Self::default()
            },
            McpOAuthSetting::Configured(oauth) => Self {
                enabled: true,
                client_id: oauth.client_id.clone().unwrap_or_default(),
                client_secret: oauth.client_secret.clone().unwrap_or_default(),
                scope: oauth.scope.clone().unwrap_or_default(),
                redirect_uri: oauth.redirect_uri.clone().unwrap_or_default(),
                callback_port: oauth
                    .callback_port
                    .map(|port| port.to_string())
                    .unwrap_or_default(),
            },
        }
    }
}

fn milliseconds(value: Option<u64>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

/// Whether `value` is an absolute http(s) address (`isAbsoluteHttpUrl`).
fn is_absolute_http_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// A timeout as the collapsed summary shows it (`formatDuration`).
fn duration_label(value: &str) -> Option<String> {
    let milliseconds: f64 = value.trim().parse().ok()?;
    if !milliseconds.is_finite() || milliseconds <= 0.0 {
        return None;
    }
    Some(if milliseconds % 1000.0 == 0.0 {
        format!("{}s", milliseconds / 1000.0)
    } else {
        format!("{milliseconds}ms")
    })
}

/// A pasted command line as its arguments (`parseShellCommand`).
fn parse_shell_command(raw: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let (mut single, mut double) = (false, false);
    for character in raw.chars() {
        match character {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            ' ' | '\t' if !single && !double => {
                if !current.is_empty() {
                    arguments.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        arguments.push(current);
    }
    arguments
}

/// Whether `line` is nothing but a link (`/^https?:\/\/\S+$/i`).
fn is_bare_link(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"));
    rest.is_some_and(|rest| !rest.is_empty() && !rest.contains(char::is_whitespace))
}

/// Whether `text` starts with a URL scheme (`/^[a-z][a-z0-9+.-]*:\/\//i`).
fn has_scheme(text: &str) -> bool {
    text.split_once("://").is_some_and(|(scheme, _)| {
        scheme
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "+.-".contains(character))
    })
}

/// A server name as typed (`normalizeMcpServerName`).
fn server_name(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '-')
            {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// `KEY=VALUE` lines of a pasted `.env` block (`handlePasteDotEnv`).
fn parse_dot_env(raw: &str) -> Vec<(String, String)> {
    let mut parsed = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, mut value) = (key.trim(), value.trim());
        for quote in ['"', '\''] {
            if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
                value = &value[1..value.len() - 1];
                break;
            }
        }
        if !key.is_empty() {
            parsed.push((key.to_owned(), value.to_owned()));
        }
    }
    parsed
}

impl Form {
    fn of(config: &McpServerConfig) -> Self {
        let remote = config.kind == McpKind::Remote;
        Self {
            kind: config.kind,
            command: if remote {
                Vec::new()
            } else {
                config.command.clone()
            },
            url: if remote {
                config.url.clone()
            } else {
                String::new()
            },
            environment: config.environment.clone(),
            headers: if remote {
                config.headers.clone()
            } else {
                Vec::new()
            },
            timeout_startup: milliseconds(config.timeout_startup),
            timeout_catalog: milliseconds(config.timeout_catalog),
            timeout_execution: milliseconds(config.timeout_execution),
            codemode: config.codemode,
            protocol: config.protocol,
            metadata_url: match (&config.oauth, remote) {
                (McpOAuthSetting::Configured(oauth), true) => {
                    oauth.auth_server_metadata_url.clone().unwrap_or_default()
                }
                _ => String::new(),
            },
            enabled: !config.disabled,
        }
    }

    /// The locale key of what keeps the form from being written.
    fn refusal(&self) -> Option<&'static str> {
        match self.kind {
            McpKind::Local if self.command.is_empty() => {
                Some("settings.mcp.page.toast.localCommandRequired")
            }
            McpKind::Remote if self.url.trim().is_empty() => {
                Some("settings.mcp.page.toast.remoteUrlRequired")
            }
            McpKind::Remote
                if !self.metadata_url.trim().is_empty()
                    && !is_absolute_http_url(self.metadata_url.trim()) =>
            {
                Some("settings.mcp.page.advanced.oauthMetadataUrlInvalid")
            }
            _ => None,
        }
    }

    /// The draft a write sends (`buildDraft`).
    fn draft(&self, carried: &Carried, scope: McpScope) -> McpDraft {
        McpDraft {
            scope,
            kind: self.kind,
            command: self.command.clone(),
            url: self.url.clone(),
            environment: self.environment.clone(),
            headers: self.headers.clone(),
            oauth_enabled: carried.enabled,
            oauth_client_id: carried.client_id.clone(),
            oauth_client_secret: carried.client_secret.clone(),
            oauth_scope: carried.scope.clone(),
            oauth_redirect_uri: carried.redirect_uri.clone(),
            oauth_callback_port: carried.callback_port.clone(),
            oauth_auth_server_metadata_url: self.metadata_url.clone(),
            protocol: self.protocol,
            timeout_startup: self.timeout_startup.clone(),
            timeout_catalog: self.timeout_catalog.clone(),
            timeout_execution: self.timeout_execution.clone(),
            codemode: self.codemode,
            disabled: !self.enabled,
        }
    }

    /// What is set under Advanced, so the collapsed section says it.
    fn advanced_summary(&self, strings: &locale::Strings) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.kind == McpKind::Remote && !self.headers.is_empty() {
            parts.push(format!(
                "{} {}",
                self.headers.len(),
                strings.get("settings.mcp.page.advanced.headers")
            ));
        }
        let durations = [
            (
                "settings.mcp.page.advanced.summary.startup",
                (self.kind == McpKind::Local)
                    .then(|| duration_label(&self.timeout_startup))
                    .flatten(),
            ),
            (
                "settings.mcp.page.advanced.summary.catalog",
                duration_label(&self.timeout_catalog),
            ),
            (
                "settings.mcp.page.advanced.summary.execution",
                duration_label(&self.timeout_execution),
            ),
        ];
        for (key, value) in durations {
            if let Some(value) = value {
                parts.push(strings.format(key, &[("value", &value)]).to_string());
            }
        }
        match self.codemode {
            Some(true) => parts.push(
                strings
                    .get("settings.mcp.page.advanced.summary.codemodeOn")
                    .to_string(),
            ),
            Some(false) => parts.push(
                strings
                    .get("settings.mcp.page.advanced.summary.codemodeOff")
                    .to_string(),
            ),
            None => {}
        }
        if self.protocol != McpProtocol::Legacy {
            parts.push(strings.get(protocol_label(self.protocol)).to_string());
        }
        parts.join(" · ")
    }
}

fn protocol_label(protocol: McpProtocol) -> &'static str {
    match protocol {
        McpProtocol::Legacy => "settings.mcp.page.advanced.protocolOption.legacy",
        McpProtocol::Auto => "settings.mcp.page.advanced.protocolOption.auto",
        McpProtocol::Revision20260728 => {
            "settings.mcp.page.advanced.protocolOption.revision20260728"
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Choice {
    value: String,
    label: SharedString,
}

impl SearchableListItem for Choice {
    type Value = String;
    fn title(&self) -> SharedString {
        self.label.clone()
    }
    fn value(&self) -> &String {
        &self.value
    }
}

type Picker = Entity<SelectState<SearchableVec<Choice>>>;

/// Which key and value rows an editor holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pairs {
    Environment,
    Headers,
}

impl Pairs {
    fn slug(self) -> &'static str {
        match self {
            Self::Environment => "env",
            Self::Headers => "header",
        }
    }

    /// The key as the field keeps it: an environment name in capitals, a
    /// header name without leading space.
    fn key(self, typed: &str) -> String {
        match self {
            Self::Environment => typed
                .to_uppercase()
                .chars()
                .map(|character| {
                    if character.is_ascii_uppercase()
                        || character.is_ascii_digit()
                        || character == '_'
                    {
                        character
                    } else {
                        '_'
                    }
                })
                .collect(),
            Self::Headers => typed.trim_start().to_owned(),
        }
    }
}

struct PairRow {
    key: Entity<InputState>,
    value: Entity<InputState>,
    _subscriptions: [Subscription; 2],
}

/// Where a started sign-in stands (`Flow` in `ProviderOAuthMethods.tsx`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum SignInPhase {
    Idle,
    Starting,
    /// The browser finishes it; the attempt is polled.
    Waiting(OAuthAttempt),
    AwaitingCode {
        attempt: OAuthAttempt,
        submitting: bool,
    },
    Failed(SharedString),
}

struct SignIn {
    /// The integration OpenCode registered for the server, once found.
    integration: Option<McpOAuthIntegration>,
    lookup: Option<Task<()>>,
    /// Whether "Sign in again" or "Authorize" was pressed for this server.
    requested: bool,
    phase: SignInPhase,
    code: Entity<InputState>,
    flow: Option<Task<()>>,
}

/// A connect, disconnect or test in flight for the open server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Busy {
    Connecting,
    Testing,
}

/// One open editor: a saved server, or a draft Create has not confirmed.
struct Editor {
    generation: u64,
    /// `None` for a draft.
    saved_name: Option<String>,
    /// What the config entry holds; a save writes only a difference from it.
    saved: Form,
    carried: Carried,
    scope: McpScope,
    legacy: bool,
    path: Option<String>,
    kind: McpKind,
    enabled: bool,
    codemode: Option<bool>,
    protocol: McpProtocol,
    advanced_open: bool,
    name: Entity<InputState>,
    command: Entity<TextareaState>,
    url: Entity<InputState>,
    timeout_startup: Entity<InputState>,
    timeout_catalog: Entity<InputState>,
    timeout_execution: Entity<InputState>,
    metadata_url: Entity<InputState>,
    environment: Vec<PairRow>,
    headers: Vec<PairRow>,
    scope_picker: Picker,
    protocol_picker: Picker,
    codemode_picker: Picker,
    /// The newest form handed to `SettingsWrites` and not answered yet.
    submitted: Option<Form>,
    /// Counts accepted saves; only the newest one's answer settles
    /// `submitted` and the failure flag.
    revision: u64,
    /// Create is in flight.
    creating: bool,
    sign_in: SignIn,
    _subscriptions: Vec<Subscription>,
}

impl Editor {
    fn rows(&self, kind: Pairs) -> &Vec<PairRow> {
        match kind {
            Pairs::Environment => &self.environment,
            Pairs::Headers => &self.headers,
        }
    }

    fn rows_mut(&mut self, kind: Pairs) -> &mut Vec<PairRow> {
        match kind {
            Pairs::Environment => &mut self.environment,
            Pairs::Headers => &mut self.headers,
        }
    }
}

/// What the page shows for a server's connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    /// An `integration.use` policy keeps OpenCode from loading it.
    Blocked,
    Runtime(McpConnection),
    /// OpenCode never reported on it and this page's connect failed.
    ConnectFailed,
}

pub(crate) struct McpPage {
    owner: WeakEntity<Workspace>,
    network: Option<Network>,
    target: Option<ServerTarget>,
    directory: Option<String>,
    list: Load<Vec<McpServerConfig>>,
    /// The reason OpenCode gave for each failed or unauthorized server at
    /// this page's last status read.
    reasons: HashMap<String, String>,
    /// Servers whose connect from this page failed.
    connect_failures: HashSet<String>,
    policies: Vec<IntegrationPolicy>,
    /// The selected server's name, or the draft's.
    selected: Option<String>,
    editor: Option<Editor>,
    editor_generation: u64,
    read_revision: u64,
    read: Option<Task<()>>,
    status_revision: u64,
    status_read: Option<Task<()>>,
    writes: SettingsWrites,
    save_failed: bool,
    /// Servers with a delete accepted and not answered. They take no save
    /// and no second delete.
    deleting: HashSet<String>,
    busy: Option<Busy>,
    action: Option<Task<()>>,
    /// Whether Settings shows this page. A hidden page reads nothing.
    shown: bool,
    /// Retry was pressed; the next render sends the kept edit again.
    retry_requested: bool,
    /// The open server's entry changed on disk while its form was clean; the
    /// next render shows the new entry.
    rehydrate_requested: bool,
    /// Feedback of the last action, kept for tests.
    notice: Option<SharedString>,
    search: Option<Entity<InputState>>,
    scroll: ScrollHandle,
    focus: HashMap<SharedString, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

/// The directory servers are read for, as `useSettingsDirectory` resolves it.
fn directory(owner: &WeakEntity<Workspace>, cx: &App) -> Option<String> {
    owner.upgrade()?.read(cx).config_directory()
}

impl McpPage {
    pub(crate) fn new(owner: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(workspace) = owner.upgrade() {
            subscriptions.push(cx.observe(&workspace, |this, _, cx| {
                let target = ServerTarget::capture(&this.owner, this.network.as_ref(), cx);
                let moved = !same_target(this.target.as_ref(), target.as_ref())
                    || this.directory != directory(&this.owner, cx);
                if !moved {
                    // The status store may have changed under the page.
                    if this.shown {
                        cx.notify();
                    }
                    return;
                }
                if this.shown {
                    this.refresh(cx);
                } else if this.target.is_some() {
                    // Hidden: the old owner's list and editor are let go
                    // without a read. Showing the page reads the current one.
                    this.retire();
                    this.target = None;
                    this.read = None;
                    cx.notify();
                }
            }));
        }
        Self {
            owner,
            network: Network::new().ok(),
            target: None,
            directory: None,
            list: Load::Loading,
            reasons: HashMap::new(),
            connect_failures: HashSet::new(),
            policies: Vec::new(),
            selected: None,
            editor: None,
            editor_generation: 0,
            read_revision: 0,
            read: None,
            status_revision: 0,
            status_read: None,
            writes: SettingsWrites::default(),
            save_failed: false,
            deleting: HashSet::new(),
            busy: None,
            action: None,
            shown: false,
            retry_requested: false,
            rehydrate_requested: false,
            notice: None,
            search: None,
            scroll: ScrollHandle::new(),
            focus: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Settings shows or leaves this page. Showing it reads again; leaving
    /// it returns to the server list, as the reference's page unmounts.
    pub(crate) fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.shown && !shown {
            self.close_editor(cx);
        }
        self.shown = shown;
        if shown {
            self.refresh(cx);
        }
    }

    /// Lets go of everything that belongs to the server and directory shown
    /// so far. Writes already accepted for it still run to their answer.
    fn retire(&mut self) {
        self.list = Load::Loading;
        self.reasons.clear();
        self.connect_failures.clear();
        self.policies.clear();
        self.selected = None;
        self.editor = None;
        self.save_failed = false;
        self.deleting.clear();
        self.busy = None;
        self.action = None;
        self.status_read = None;
        self.rehydrate_requested = false;
    }

    /// Reads the list for the current server and directory.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let target = ServerTarget::capture(&self.owner, self.network.as_ref(), cx);
        let directory = directory(&self.owner, cx);
        if !same_target(self.target.as_ref(), target.as_ref()) || self.directory != directory {
            // Another server's or project's entries never carry over. A
            // sign-in started there is dropped with its editor.
            self.cancel_sign_in_upstream();
            self.retire();
        }
        self.target = target;
        self.directory = directory;
        if self.list == Load::Failed {
            self.list = Load::Loading;
        }
        self.read_list(cx);
    }

    fn ticket(&self, cx: &App) -> Option<RefreshTicket> {
        let directory = self.directory.as_deref()?;
        Some(
            self.owner
                .upgrade()?
                .read(cx)
                .mcp_status
                .store
                .begin_refresh(directory),
        )
    }

    /// Takes a status read's answer, `None` when it failed: the statuses go
    /// to the store the work status card shows, the reasons stay here.
    fn land_reports(
        &mut self,
        ticket: Option<RefreshTicket>,
        reports: Option<Vec<McpServerReport>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(reports) = &reports {
            self.reasons = reports
                .iter()
                .filter_map(|report| Some((report.status.name.clone(), report.error.clone()?)))
                .collect();
            // OpenCode's own report replaces this page's guess.
            self.connect_failures
                .retain(|name| !reports.iter().any(|report| &report.status.name == name));
        }
        let (Some(owner), Some(ticket)) = (self.owner.upgrade(), ticket) else {
            return;
        };
        let statuses = reports.map(|reports| {
            reports
                .into_iter()
                .map(|report| report.status)
                .collect::<Vec<_>>()
        });
        owner.update(cx, |workspace, cx| {
            if workspace
                .mcp_status
                .store
                .finish_refresh(&ticket, statuses, Instant::now())
            {
                cx.notify();
            }
        });
    }

    /// Reads the config entries, the statuses and the policies. A list
    /// already shown stays through a failed reread; a first read fails
    /// visibly and never shows as an empty list.
    fn read_list(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Some(directory)) = (self.target.clone(), self.directory.clone()) else {
            self.read = None;
            cx.notify();
            return;
        };
        self.read_revision += 1;
        let revision = self.read_revision;
        let ticket = self.ticket(cx);
        let client = target.client;
        let reading = NetworkTask::spawn_on(&target.network, async move {
            tokio::join!(
                client.mcp_configs(&directory),
                client.mcp_server_reports(&directory),
                client.integration_policies(&directory),
            )
        });
        self.read = Some(cx.spawn(async move |this, cx| {
            let result = reading.await;
            let _ = this.update(cx, |this, cx| {
                if this.read_revision != revision {
                    return;
                }
                this.read = None;
                let Some((configs, reports, policies)) = result else {
                    if this.list.ready().is_none() {
                        this.list = Load::Failed;
                    }
                    cx.notify();
                    return;
                };
                this.land_reports(ticket, reports.ok(), cx);
                // A failed policy read leaves every row unmarked.
                if let Ok(policies) = policies {
                    this.policies = policies;
                }
                match configs {
                    Ok(configs) => {
                        this.follow_stored_entry(&configs);
                        this.list = Load::Ready(configs);
                    }
                    Err(_) if this.list.ready().is_some() => {}
                    Err(_) => this.list = Load::Failed,
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Keeps the open editor in step with its entry on disk. What the page
    /// cannot edit always follows; the form follows only while it is clean,
    /// so an edit in progress is never replaced.
    fn follow_stored_entry(&mut self, configs: &[McpServerConfig]) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let Some(config) = configs
            .iter()
            .find(|config| Some(&config.name) == editor.saved_name.as_ref())
        else {
            return;
        };
        editor.carried = Carried::of(&config.oauth);
        editor.legacy = config.legacy;
        editor.path = config.path.clone();
        if editor.submitted.is_none() && Form::of(config) != editor.saved {
            self.rehydrate_requested = true;
        }
    }

    /// Reads the statuses alone, after something this page did.
    fn read_status(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Some(directory)) = (self.target.clone(), self.directory.clone()) else {
            return;
        };
        self.status_revision += 1;
        let revision = self.status_revision;
        let ticket = self.ticket(cx);
        let client = target.client;
        let reading = NetworkTask::spawn_on(&target.network, async move {
            client.mcp_server_reports(&directory).await
        });
        self.status_read = Some(cx.spawn(async move |this, cx| {
            let result = reading.await;
            let _ = this.update(cx, |this, cx| {
                if this.status_revision != revision {
                    return;
                }
                this.status_read = None;
                this.land_reports(ticket, result.and_then(Result::ok), cx);
                cx.notify();
            });
        }));
    }

    fn servers(&self) -> &[McpServerConfig] {
        self.list.ready().map_or(&[], Vec::as_slice)
    }

    fn connection(&self, name: &str, cx: &App) -> Option<McpConnection> {
        let directory = self.directory.as_deref()?;
        self.owner
            .upgrade()?
            .read(cx)
            .mcp_status
            .store
            .servers(directory)
            .iter()
            .find(|server| server.name == name)
            .map(|server| server.connection)
    }

    fn is_blocked(&self, name: &str) -> bool {
        is_integration_denied(&self.policies, &format!("mcp:{name}"))
    }

    /// What the page shows for `name`: a policy block first, then OpenCode's
    /// own report, then this page's failed connect.
    fn standing(&self, name: &str, cx: &App) -> Option<Standing> {
        if self.is_blocked(name) {
            return Some(Standing::Blocked);
        }
        self.connection(name, cx).map(Standing::Runtime).or(self
            .connect_failures
            .contains(name)
            .then_some(Standing::ConnectFailed))
    }

    /// The servers OpenCode runs that no config file defines, known once the
    /// config list is read.
    fn runtime_only(&self, cx: &App) -> Vec<(String, McpConnection)> {
        let (Some(configs), Some(directory)) = (self.list.ready(), self.directory.as_deref())
        else {
            return Vec::new();
        };
        let Some(owner) = self.owner.upgrade() else {
            return Vec::new();
        };
        owner
            .read(cx)
            .mcp_status
            .store
            .servers(directory)
            .iter()
            .filter(|server| !configs.iter().any(|config| config.name == server.name))
            .map(|server| (server.name.clone(), server.connection))
            .collect()
    }

    fn form(&self, cx: &App) -> Option<Form> {
        let editor = self.editor.as_ref()?;
        let text = |state: &Entity<InputState>| state.read(cx).value().to_string();
        let pairs = |kind: Pairs| -> Vec<(String, String)> {
            editor
                .rows(kind)
                .iter()
                .map(|row| (kind.key(&text(&row.key)), text(&row.value)))
                .filter(|(key, _)| !key.trim().is_empty())
                .collect()
        };
        let remote = editor.kind == McpKind::Remote;
        Some(Form {
            kind: editor.kind,
            command: editor
                .command
                .read(cx)
                .value()
                .lines()
                .map(str::trim_end)
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned)
                .collect(),
            url: text(&editor.url),
            environment: pairs(Pairs::Environment),
            headers: pairs(Pairs::Headers),
            timeout_startup: text(&editor.timeout_startup),
            timeout_catalog: text(&editor.timeout_catalog),
            timeout_execution: text(&editor.timeout_execution),
            codemode: editor.codemode,
            protocol: editor.protocol,
            metadata_url: if remote {
                text(&editor.metadata_url)
            } else {
                String::new()
            },
            enabled: editor.enabled,
        })
    }

    /// A name no server of the list carries (`nextServerName`).
    fn next_server_name(&self) -> String {
        let taken = |name: &str| self.servers().iter().any(|server| server.name == name);
        let mut name = "new-mcp-server".to_owned();
        let mut counter = 1;
        while taken(&name) {
            name = format!("new-mcp-server-{counter}");
            counter += 1;
        }
        name
    }

    fn create_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.list.ready().is_none() {
            return;
        }
        self.request_save(window, cx);
        self.close_editor(cx);
        let name = self.next_server_name();
        self.open_editor(None, &name, window, cx);
        self.selected = Some(name);
    }

    fn select(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(name) && self.editor.is_some() {
            return;
        }
        let Some(config) = self
            .servers()
            .iter()
            .find(|server| server.name == name)
            .cloned()
        else {
            return;
        };
        // Text still focused in the editor being left is committed first.
        self.request_save(window, cx);
        self.close_editor(cx);
        self.open_editor(Some(&config), "", window, cx);
        self.selected = Some(config.name);
    }

    /// Back to the server list. Pending text of the open server is sent.
    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.request_save(window, cx);
        self.close_editor(cx);
    }

    /// Asks OpenCode to drop a sign-in nobody follows any more.
    fn cancel_sign_in_upstream(&mut self) {
        let (Some(editor), Some(target), Some(directory)) =
            (&mut self.editor, &self.target, &self.directory)
        else {
            return;
        };
        let attempt = match &editor.sign_in.phase {
            SignInPhase::Waiting(attempt) | SignInPhase::AwaitingCode { attempt, .. } => {
                attempt.id.clone()
            }
            _ => return,
        };
        editor.sign_in.phase = SignInPhase::Idle;
        editor.sign_in.flow = None;
        let Some(integration) = &editor.sign_in.integration else {
            return;
        };
        let (client, integration, directory) = (
            target.client.clone(),
            integration.id.clone(),
            directory.clone(),
        );
        // The request outlives the editor; nothing waits for its answer.
        drop(target.network.spawn(async move {
            let _ = client
                .cancel_mcp_oauth(&integration, &attempt, &directory)
                .await;
        }));
    }

    fn close_editor(&mut self, cx: &mut Context<Self>) {
        self.cancel_sign_in_upstream();
        self.editor = None;
        self.selected = None;
        self.save_failed = false;
        self.busy = None;
        self.action = None;
        self.rehydrate_requested = false;
        cx.notify();
    }

    fn pair_row(
        kind: Pairs,
        key: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PairRow {
        let strings = locale::strings(cx).clone();
        let key_placeholder = strings.get(match kind {
            Pairs::Environment => "settings.mcp.page.env.keyPlaceholder",
            Pairs::Headers => "settings.mcp.page.advanced.headerNamePlaceholder",
        });
        let (key, value) = (key.to_owned(), value.to_owned());
        let key_state = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_placeholder(key_placeholder, window, cx);
            state.set_value(key, window, cx);
            state
        });
        let value_state = cx.new(|cx| {
            let mut state = InputState::new(window, cx).masked(true);
            state.set_placeholder(
                strings.get("settings.mcp.page.env.valuePlaceholder"),
                window,
                cx,
            );
            state.set_value(value, window, cx);
            state
        });
        let subscriptions = [
            cx.subscribe_in(
                &key_state,
                window,
                move |this, input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Blur) {
                        let typed = input.read(cx).value().to_string();
                        let kept = kind.key(&typed);
                        if kept != typed {
                            input.update(cx, |input, cx| input.set_value(kept, window, cx));
                        }
                        this.request_save(window, cx);
                    }
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &value_state,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Blur) {
                        this.request_save(window, cx);
                    }
                    cx.notify();
                },
            ),
        ];
        PairRow {
            key: key_state,
            value: value_state,
            _subscriptions: subscriptions,
        }
    }

    fn add_pair(
        &mut self,
        kind: Pairs,
        key: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row = Self::pair_row(kind, key, value, window, cx);
        if let Some(editor) = &mut self.editor {
            editor.rows_mut(kind).push(row);
            cx.notify();
        }
    }

    fn remove_pair(
        &mut self,
        kind: Pairs,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = &mut self.editor
            && index < editor.rows(kind).len()
        {
            editor.rows_mut(kind).remove(index);
            // A removed row has no field to leave, so it is written at once.
            self.request_save(window, cx);
            cx.notify();
        }
    }

    /// Merges `KEY=VALUE` lines from the clipboard into the rows.
    fn paste_pairs(&mut self, kind: Pairs, window: &mut Window, cx: &mut Context<Self>) {
        let strings = locale::strings(cx).clone();
        let Some(raw) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            let text = strings.get("settings.mcp.page.toast.clipboardReadFailed");
            self.toast(text, false, window, cx);
            return;
        };
        let parsed = parse_dot_env(&raw);
        if parsed.is_empty() {
            let text = strings.get("settings.mcp.page.toast.noKeyValuePairsFound");
            self.toast(text, false, window, cx);
            return;
        }
        let count = parsed.len().to_string();
        for (key, value) in parsed {
            let existing = self.editor.as_ref().and_then(|editor| {
                editor
                    .rows(kind)
                    .iter()
                    .find(|row| row.key.read(cx).value().as_ref() == key.as_str())
                    .map(|row| row.value.clone())
            });
            match existing {
                Some(field) => field.update(cx, |input, cx| input.set_value(value, window, cx)),
                None => self.add_pair(kind, &key, &value, window, cx),
            }
        }
        self.request_save(window, cx);
        let text = strings.format(
            "settings.mcp.page.toast.importedVariablesCount",
            &[("count", &count)],
        );
        self.toast(text, true, window, cx);
    }

    /// Opens the editor on a saved server, or on a draft named `draft_name`.
    fn open_editor(
        &mut self,
        saved: Option<&McpServerConfig>,
        draft_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let strings = locale::strings(cx).clone();
        let form = saved.map(Form::of).unwrap_or_else(|| Form {
            enabled: true,
            ..Form::default()
        });
        let input =
            |value: &str, placeholder: SharedString, window: &mut Window, cx: &mut Context<Self>| {
                let value = value.to_owned();
                cx.new(|cx| {
                    let mut state = InputState::new(window, cx);
                    state.set_placeholder(placeholder, window, cx);
                    state.set_value(value, window, cx);
                    state
                })
            };
        let name = input(
            draft_name,
            strings.get("settings.mcp.page.server.namePlaceholder"),
            window,
            cx,
        );
        let url = input(
            &form.url,
            strings.get("settings.mcp.page.connection.serverUrlPlaceholder"),
            window,
            cx,
        );
        let timeout_startup = input(&form.timeout_startup, "5000".into(), window, cx);
        let timeout_catalog = input(&form.timeout_catalog, "30000".into(), window, cx);
        let timeout_execution = input(&form.timeout_execution, "30000".into(), window, cx);
        let metadata_url = input(
            &form.metadata_url,
            strings.get("settings.mcp.page.advanced.oauthMetadataUrlPlaceholder"),
            window,
            cx,
        );
        let code = input(
            "",
            strings.get("settings.providers.page.auth.pasteAuthorizationCodePlaceholder"),
            window,
            cx,
        );
        let command = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx).auto_grow(4, 16).placeholder(
                "npx\n-y\n@modelcontextprotocol/server-postgres\npostgresql://user:pass@host/db",
            );
            state.set_value(form.command.join("\n"), window, cx);
            state
        });
        let picker = |choices: Vec<Choice>,
                      current: &str,
                      window: &mut Window,
                      cx: &mut Context<Self>|
         -> Picker {
            let selected = choices
                .iter()
                .position(|choice| choice.value == current)
                .unwrap_or(0);
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(choices),
                    Some(IndexPath::new(selected)),
                    window,
                    cx,
                )
            })
        };
        let scope = saved
            .and_then(|config| config.scope)
            .unwrap_or(McpScope::User);
        let scope_picker = picker(
            vec![
                Choice {
                    value: "user".into(),
                    label: strings.get("settings.mcp.page.scope.everywhere"),
                },
                Choice {
                    value: "project".into(),
                    label: strings.get("settings.mcp.page.scope.thisProject"),
                },
            ],
            "user",
            window,
            cx,
        );
        let protocol_picker = picker(
            McpProtocol::ALL
                .into_iter()
                .map(|protocol| Choice {
                    value: protocol.as_wire().to_owned(),
                    label: strings.get(protocol_label(protocol)),
                })
                .collect(),
            form.protocol.as_wire(),
            window,
            cx,
        );
        let codemode_value = |codemode: Option<bool>| match codemode {
            None => "default",
            Some(true) => "on",
            Some(false) => "off",
        };
        let codemode_picker = picker(
            ["default", "on", "off"]
                .into_iter()
                .map(|value| Choice {
                    value: value.to_owned(),
                    label: strings.get(&format!(
                        "settings.mcp.page.advanced.codemodeOption.{value}"
                    )),
                })
                .collect(),
            codemode_value(form.codemode),
            window,
            cx,
        );
        let save_on_blur = |state: &Entity<InputState>,
                            window: &mut Window,
                            cx: &mut Context<Self>| {
            cx.subscribe_in(state, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Blur) {
                    this.request_save(window, cx);
                }
                cx.notify();
            })
        };
        let subscriptions = vec![
            cx.subscribe_in(&name, window, |_, input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    let typed = input.read(cx).value().to_string();
                    let kept = server_name(&typed);
                    if kept != typed {
                        input.update(cx, |input, cx| input.set_value(kept, window, cx));
                    }
                }
                cx.notify();
            }),
            cx.subscribe_in(&command, window, |this, _, event, window, cx| {
                match event {
                    InputEvent::Change => this.follow_command_text(window, cx),
                    InputEvent::Blur => this.request_save(window, cx),
                    _ => {}
                }
                cx.notify();
            }),
            cx.subscribe_in(&url, window, |this, _, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::Change => this.follow_url_text(window, cx),
                    InputEvent::Blur => this.request_save(window, cx),
                    _ => {}
                }
                cx.notify();
            }),
            save_on_blur(&timeout_startup, window, cx),
            save_on_blur(&timeout_catalog, window, cx),
            save_on_blur(&timeout_execution, window, cx),
            save_on_blur(&metadata_url, window, cx),
            cx.subscribe_in(&code, window, |_, _, _: &InputEvent, _, cx| cx.notify()),
            cx.subscribe_in(&scope_picker, window, |this, _, event, _, cx| {
                if let SelectEvent::Confirm(Some(value)) = event
                    && let Some(editor) = &mut this.editor
                {
                    editor.scope = if value == "project" {
                        McpScope::Project
                    } else {
                        McpScope::User
                    };
                    cx.notify();
                }
            }),
            cx.subscribe_in(&protocol_picker, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(value)) = event
                    && let Some(protocol) = McpProtocol::from_wire(value)
                    && let Some(editor) = &mut this.editor
                {
                    editor.protocol = protocol;
                    this.request_save(window, cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&codemode_picker, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(value)) = event
                    && let Some(editor) = &mut this.editor
                {
                    editor.codemode = match value.as_str() {
                        "on" => Some(true),
                        "off" => Some(false),
                        _ => None,
                    };
                    this.request_save(window, cx);
                    cx.notify();
                }
            }),
        ];
        let environment = form
            .environment
            .iter()
            .map(|(key, value)| Self::pair_row(Pairs::Environment, key, value, window, cx))
            .collect();
        let headers = form
            .headers
            .iter()
            .map(|(key, value)| Self::pair_row(Pairs::Headers, key, value, window, cx))
            .collect();
        self.editor_generation += 1;
        self.save_failed = false;
        self.busy = None;
        self.action = None;
        self.editor = Some(Editor {
            generation: self.editor_generation,
            saved_name: saved.map(|config| config.name.clone()),
            carried: saved
                .map(|config| Carried::of(&config.oauth))
                .unwrap_or_default(),
            scope,
            legacy: saved.is_some_and(|config| config.legacy),
            path: saved.and_then(|config| config.path.clone()),
            kind: form.kind,
            enabled: form.enabled,
            codemode: form.codemode,
            protocol: form.protocol,
            advanced_open: false,
            saved: form,
            name,
            command,
            url,
            timeout_startup,
            timeout_catalog,
            timeout_execution,
            metadata_url,
            environment,
            headers,
            scope_picker,
            protocol_picker,
            codemode_picker,
            submitted: None,
            revision: 0,
            creating: false,
            sign_in: SignIn {
                integration: None,
                lookup: None,
                requested: false,
                phase: SignInPhase::Idle,
                code,
                flow: None,
            },
            _subscriptions: subscriptions,
        });
        self.scroll.set_offset(gpui_kit::point(px(0.0), px(0.0)));
        self.find_integration(cx);
        cx.notify();
    }

    /// A single line that is nothing but a link is a hosted server: the
    /// editor switches kind and moves the text to the link field.
    fn follow_command_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let text = editor.command.read(cx).value().to_string();
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());
        let (Some(line), None) = (lines.next(), lines.next()) else {
            return;
        };
        if !is_bare_link(line.trim()) {
            return;
        }
        let link = line.trim().to_owned();
        editor.kind = McpKind::Remote;
        let (command, url) = (editor.command.clone(), editor.url.clone());
        url.update(cx, |input, cx| input.set_value(link, window, cx));
        command.update(cx, |input, cx| input.set_value("", window, cx));
    }

    /// A command pasted into the link field is still a command.
    fn follow_url_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let text = editor.url.read(cx).value().to_string();
        let trimmed = text.trim();
        if trimmed.is_empty() || has_scheme(trimmed) || !trimmed.contains(char::is_whitespace) {
            return;
        }
        let arguments = parse_shell_command(trimmed).join("\n");
        editor.kind = McpKind::Local;
        let (command, url) = (editor.command.clone(), editor.url.clone());
        command.update(cx, |input, cx| input.set_value(arguments, window, cx));
        url.update(cx, |input, cx| input.set_value("", window, cx));
    }

    fn set_kind(&mut self, kind: McpKind, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.editor {
            editor.kind = kind;
            self.request_save(window, cx);
            cx.notify();
        }
    }

    fn set_enabled(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.editor {
            editor.enabled = enabled;
            self.request_save(window, cx);
            cx.notify();
        }
    }

    fn toast(
        &mut self,
        text: SharedString,
        success: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notice = Some(text.clone());
        let notification = if success {
            Notification::success(text)
        } else {
            Notification::error(text)
        };
        window.push_notification(notification, cx);
    }

    /// Feedback that is neither a success nor a failure.
    fn say(&mut self, text: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        self.notice = Some(text.clone());
        window.push_notification(Notification::info(text), cx);
    }

    /// Whether a completion still belongs to the connection and directory
    /// that started it.
    fn current(&self, target: &ServerTarget, directory: &str, cx: &App) -> bool {
        same_target(self.target.as_ref(), Some(target))
            && same_target(
                ServerTarget::capture(&self.owner, self.network.as_ref(), cx).as_ref(),
                Some(target),
            )
            && self.directory.as_deref() == Some(directory)
    }

    /// Autosave of an existing server (`save`). A form that differs from the
    /// last one submitted is accepted as a write of its own at once, also
    /// while an earlier one is in flight; the server gets them in order. A
    /// failed write keeps the edit, and the next request or Retry sends it
    /// again. A server with a delete queued accepts nothing.
    pub(crate) fn request_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() {
            return;
        }
        let (Some(form), Some(target), Some(directory)) =
            (self.form(cx), self.target.clone(), self.directory.clone())
        else {
            return;
        };
        let Some(editor) = &mut self.editor else {
            return;
        };
        let Some(name) = editor.saved_name.clone() else {
            return;
        };
        if self.deleting.contains(&name) {
            return;
        }
        let in_flight = editor.submitted.is_some();
        let baseline = editor.submitted.as_ref().unwrap_or(&editor.saved);
        if &form == baseline {
            if !in_flight {
                self.save_failed = false;
            }
            return;
        }
        if let Some(key) = form.refusal() {
            self.save_failed = true;
            let text = locale::strings(cx).get(key);
            self.toast(text, false, window, cx);
            return;
        }
        editor.revision += 1;
        let (generation, revision) = (editor.generation, editor.revision);
        editor.submitted = Some(form.clone());
        let draft = form.draft(&editor.carried, editor.scope);
        let client = target.client.clone();
        let (write_name, write_directory) = (name.clone(), directory.clone());
        let (id, writing) = self.writes.submit(&target, async move {
            client
                .update_mcp_config(&write_name, &draft, &write_directory)
                .await
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<(), McpConfigError>> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if !this.current(&target, &directory, cx) {
                    return;
                }
                let saved = matches!(result, Some(Ok(())));
                if saved {
                    // The entry and what OpenCode now runs both changed.
                    this.read_list(cx);
                }
                // An editor left meanwhile took its edit with it, as in the
                // reference; its failed write is still reported below.
                if let Some(editor) = this
                    .editor
                    .as_mut()
                    .filter(|editor| editor.generation == generation)
                {
                    if saved {
                        editor.saved = form;
                    }
                    // Only the newest accepted form settles the editor: an
                    // older answer leaves the newer one in flight.
                    if editor.revision == revision {
                        editor.submitted = None;
                        this.save_failed = !saved;
                    }
                }
                if !saved {
                    let text = locale::strings(cx).get("settings.mcp.page.toast.saveFailed");
                    this.toast(text, false, window, cx);
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    /// Creates the draft (`handleCreate`).
    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() {
            return;
        }
        let (Some(form), Some(target), Some(directory)) =
            (self.form(cx), self.target.clone(), self.directory.clone())
        else {
            return;
        };
        let Some(editor) = &self.editor else {
            return;
        };
        if editor.saved_name.is_some() || editor.creating {
            return;
        }
        let name = editor.name.read(cx).value().trim().to_owned();
        let strings = locale::strings(cx).clone();
        let refusal = if name.is_empty() {
            Some("settings.mcp.page.toast.nameRequired")
        } else if self.servers().iter().any(|server| server.name == name) {
            Some("settings.mcp.page.toast.serverNameExists")
        } else {
            form.refusal()
        };
        if let Some(key) = refusal {
            self.toast(strings.get(key), false, window, cx);
            return;
        }
        let editor = self.editor.as_mut().expect("checked above");
        editor.creating = true;
        let generation = editor.generation;
        let draft = form.draft(&editor.carried, editor.scope);
        let client = target.client.clone();
        let (write_name, write_directory) = (name.clone(), directory.clone());
        let (id, writing) = self.writes.submit(&target, async move {
            client
                .create_mcp_config(&write_name, &draft, &write_directory)
                .await
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<Option<String>, McpConfigError>> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if !this.current(&target, &directory, cx) {
                    return;
                }
                let strings = locale::strings(cx).clone();
                let created = matches!(result, Some(Ok(_)));
                if created {
                    this.read_list(cx);
                }
                let open = this
                    .editor
                    .as_mut()
                    .filter(|editor| editor.generation == generation);
                if let Some(editor) = open {
                    editor.creating = false;
                    if created {
                        // The draft is now the saved server it created.
                        editor.saved_name = Some(name.clone());
                        editor.saved = form;
                        this.selected = Some(name.clone());
                        this.find_integration(cx);
                    }
                }
                match result {
                    Some(Ok(message)) => {
                        let text = message.map(SharedString::from).unwrap_or_else(|| {
                            strings.get("settings.mcp.page.toast.serverCreatedReloading")
                        });
                        this.toast(text, true, window, cx);
                    }
                    // A refused create carries the server's reason.
                    Some(Err(error)) => {
                        let text = error
                            .message()
                            .map(|message| SharedString::from(message.to_owned()))
                            .unwrap_or_else(|| {
                                strings.get("settings.mcp.page.toast.unexpectedError")
                            });
                        this.toast(text, false, window, cx);
                    }
                    None => {
                        let text = strings.get("settings.mcp.page.toast.unexpectedError");
                        this.toast(text, false, window, cx);
                    }
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    /// Asks before removing a server from its config file.
    fn ask_delete(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let strings = locale::strings(cx);
        let description = format!(
            "{} opencode.json. {}",
            strings.get("settings.mcp.page.deleteDialog.descriptionPrefix"),
            strings.get("settings.mcp.page.deleteDialog.descriptionSuffix"),
        );
        confirm(
            cx.weak_entity(),
            strings.format("settings.mcp.page.deleteDialog.title", &[("name", &name)]),
            description.into(),
            strings.get("settings.common.actions.delete"),
            window,
            cx,
            move |this, window, cx| this.delete(name.clone(), window, cx),
        );
    }

    /// Deletes a server. From here until the answer it takes no save and no
    /// second delete: a save queued behind the delete would fail on a server
    /// that is gone, or write one another create put there meanwhile.
    fn delete(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() || self.deleting.contains(&name) {
            return;
        }
        let (Some(target), Some(directory)) = (self.target.clone(), self.directory.clone()) else {
            return;
        };
        self.deleting.insert(name.clone());
        let client = target.client.clone();
        let (removed, write_directory) = (name.clone(), directory.clone());
        let (id, writing) = self.writes.submit(&target, async move {
            client.delete_mcp_config(&removed, &write_directory).await
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<Option<String>, McpConfigError>> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if !this.current(&target, &directory, cx) {
                    return;
                }
                this.deleting.remove(&name);
                let strings = locale::strings(cx).clone();
                match result {
                    Some(Ok(message)) => {
                        if this.selected.as_deref() == Some(name.as_str()) {
                            this.close_editor(cx);
                        }
                        let text = message.map(SharedString::from).unwrap_or_else(|| {
                            strings.format(
                                "settings.mcp.page.toast.serverDeleted",
                                &[("name", &name)],
                            )
                        });
                        this.toast(text, true, window, cx);
                        this.read_list(cx);
                    }
                    _ => {
                        let text = strings.get("settings.mcp.page.toast.deleteFailed");
                        this.toast(text, false, window, cx);
                    }
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    /// The open saved server with what a live action needs.
    fn live_subject(&self) -> Option<(String, ServerTarget, String)> {
        Some((
            self.editor.as_ref()?.saved_name.clone()?,
            self.target.clone()?,
            self.directory.clone()?,
        ))
    }

    /// Says how a connect ended, from the status read after it.
    fn report_connection(
        &mut self,
        name: &str,
        connected_key: &'static str,
        finished_key: &'static str,
        failed_key: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let strings = locale::strings(cx).clone();
        match self.connection(name, cx) {
            Some(McpConnection::Connected) => {
                self.toast(strings.get(connected_key), true, window, cx)
            }
            Some(McpConnection::NeedsAuth) => {
                let text = strings.get("settings.mcp.page.toast.connectionNeedsAuthorization");
                self.say(text, window, cx);
            }
            Some(McpConnection::Failed) => {
                let text = self
                    .reasons
                    .get(name)
                    .map(|reason| SharedString::from(reason.clone()))
                    .unwrap_or_else(|| strings.get(failed_key));
                self.toast(text, false, window, cx);
            }
            _ if self.connect_failures.contains(name) => {
                self.toast(strings.get(failed_key), false, window, cx)
            }
            _ => self.say(strings.get(finished_key), window, cx),
        }
    }

    /// Connect, or Disconnect for a connected server (`handleToggleConnect`).
    fn toggle_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((name, target, directory)) = self.live_subject() else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        let connected = self.connection(&name, cx) == Some(McpConnection::Connected);
        self.busy = Some(Busy::Connecting);
        let ticket = self.ticket(cx);
        let client = target.client.clone();
        let (server, read_directory) = (name.clone(), directory.clone());
        let work = NetworkTask::spawn_on(&target.network, async move {
            let outcome = if connected {
                client.disconnect_mcp_server(&server, &read_directory).await
            } else {
                client.connect_mcp_server(&server, &read_directory).await
            };
            let reports = client.mcp_server_reports(&read_directory).await.ok();
            (outcome.is_ok(), reports)
        });
        self.action = Some(cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.current(&target, &directory, cx) {
                    return;
                }
                this.busy = None;
                let strings = locale::strings(cx).clone();
                let Some((succeeded, reports)) = result else {
                    return;
                };
                if !connected && !succeeded {
                    this.connect_failures.insert(name.clone());
                }
                this.land_reports(ticket, reports, cx);
                if connected {
                    let key = if succeeded {
                        "settings.mcp.page.toast.disconnected"
                    } else {
                        "settings.mcp.page.toast.connectionFailed"
                    };
                    this.toast(strings.get(key), succeeded, window, cx);
                } else if !succeeded {
                    let text = strings.get("settings.mcp.page.toast.connectionFailed");
                    this.toast(text, false, window, cx);
                } else {
                    this.report_connection(
                        &name,
                        "settings.mcp.page.toast.connected",
                        "settings.mcp.page.toast.connectionAttemptFinished",
                        "settings.mcp.page.toast.connectionFailed",
                        window,
                        cx,
                    );
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Connects once to see what happens (`testConnection`). Only a server
    /// the user turned off goes back off after the test.
    fn test_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((name, target, directory)) = self.live_subject() else {
            let text = locale::strings(cx)
                .get("settings.mcp.page.toast.createServerBeforeLiveActions");
            self.toast(text, false, window, cx);
            return;
        };
        if self.busy.is_some() {
            return;
        }
        if self.editor.as_ref().is_some_and(|editor| !editor.enabled) {
            let text = locale::strings(cx).get("settings.mcp.page.toast.enableServerBeforeTest");
            self.toast(text, false, window, cx);
            return;
        }
        let was_disabled = self.connection(&name, cx) == Some(McpConnection::Disabled);
        self.busy = Some(Busy::Testing);
        let ticket = self.ticket(cx);
        let client = target.client.clone();
        let (server, read_directory) = (name.clone(), directory.clone());
        let work = NetworkTask::spawn_on(&target.network, async move {
            let connected = client
                .connect_mcp_server(&server, &read_directory)
                .await
                .is_ok();
            let observed = client.mcp_server_reports(&read_directory).await.ok();
            let runs = observed.as_ref().is_some_and(|reports| {
                reports.iter().any(|report| {
                    report.status.name == server
                        && report.status.connection == McpConnection::Connected
                })
            });
            if !(was_disabled && runs) {
                return (connected, observed, None, None);
            }
            let cleanup = client
                .disconnect_mcp_server(&server, &read_directory)
                .await
                .err()
                .map(|error| error.to_string());
            let after = client.mcp_server_reports(&read_directory).await.ok();
            (connected, observed, cleanup, after)
        });
        self.action = Some(cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.current(&target, &directory, cx) {
                    return;
                }
                this.busy = None;
                let Some((connected, observed, cleanup, after)) = result else {
                    return;
                };
                if !connected {
                    this.connect_failures.insert(name.clone());
                }
                // The outcome is what the test saw, before any cleanup.
                let seen = observed.as_ref().and_then(|reports| {
                    reports
                        .iter()
                        .find(|report| report.status.name == name)
                        .cloned()
                });
                let strings = locale::strings(cx).clone();
                this.land_reports(ticket, after.or(observed), cx);
                let failed_text = strings.get("settings.mcp.page.toast.connectionTestFailed");
                let seen_connection = seen.as_ref().map(|report| report.status.connection);
                match (cleanup, seen_connection) {
                    // The reference words this one in English only.
                    (Some(error), _) => {
                        let text: SharedString = format!(
                            "Connection test succeeded, but cleanup disconnect failed: {error}"
                        )
                        .into();
                        this.notice = Some(text.clone());
                        window.push_notification(Notification::warning(text), cx);
                    }
                    (None, Some(McpConnection::Connected)) => {
                        let text = strings.get("settings.mcp.page.toast.connectionTestSucceeded");
                        this.toast(text, true, window, cx);
                    }
                    (None, Some(McpConnection::NeedsAuth)) => {
                        let text =
                            strings.get("settings.mcp.page.toast.connectionNeedsAuthorization");
                        this.say(text, window, cx);
                    }
                    (None, Some(McpConnection::Failed)) => {
                        let text = seen
                            .and_then(|report| report.error)
                            .map(SharedString::from)
                            .unwrap_or(failed_text);
                        this.toast(text, false, window, cx);
                    }
                    (None, _) if !connected => this.toast(failed_text, false, window, cx),
                    (None, _) => {
                        let text = strings.get("settings.mcp.page.toast.connectionTestFinished");
                        this.say(text, window, cx);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Refresh in the list's header: status again, and a failure says so.
    fn refresh_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(target), Some(directory)) = (self.target.clone(), self.directory.clone()) else {
            return;
        };
        self.status_revision += 1;
        let revision = self.status_revision;
        let ticket = self.ticket(cx);
        let client = target.client;
        let reading = NetworkTask::spawn_on(&target.network, async move {
            client.mcp_server_reports(&directory).await
        });
        self.status_read = Some(cx.spawn_in(window, async move |this, cx| {
            let result = reading.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.status_revision != revision {
                    return;
                }
                this.status_read = None;
                let reports = result.and_then(Result::ok);
                if reports.is_none() {
                    let text = locale::strings(cx).get("settings.mcp.page.toast.refreshStatusFailed");
                    this.toast(text, false, window, cx);
                }
                this.land_reports(ticket, reports, cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Looks for the OAuth sign-in OpenCode registered for the open server.
    /// A remote server with OAuth left on has one, and so may a failed one.
    fn find_integration(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Some(directory)) = (self.target.clone(), self.directory.clone()) else {
            return;
        };
        let Some(editor) = &mut self.editor else {
            return;
        };
        let Some(name) = editor.saved_name.clone() else {
            return;
        };
        let failed = self
            .owner
            .upgrade()
            .map(|owner| {
                owner
                    .read(cx)
                    .mcp_status
                    .store
                    .servers(&directory)
                    .iter()
                    .any(|server| {
                        server.name == name && server.connection == McpConnection::Failed
                    })
            })
            .unwrap_or_default();
        let configured = editor.saved.kind == McpKind::Remote && editor.carried.enabled;
        if !(configured || failed) || editor.sign_in.integration.is_some() {
            return;
        }
        let generation = editor.generation;
        editor.sign_in.lookup = Some(cx.spawn(async move |this, cx| {
            for attempt in 0..INTEGRATION_LOOKUPS {
                if attempt > 0 {
                    cx.background_executor()
                        .timer(INTEGRATION_LOOKUP_INTERVAL)
                        .await;
                }
                let (client, server, read_directory) =
                    (target.client.clone(), name.clone(), directory.clone());
                let found = NetworkTask::spawn_on(&target.network, async move {
                    client.mcp_oauth_integration(&server, &read_directory).await
                })
                .await;
                let Some(Ok(Some(integration))) = found else {
                    continue;
                };
                let _ = this.update(cx, |this, cx| {
                    if !this.current(&target, &directory, cx) {
                        return;
                    }
                    if let Some(editor) = this
                        .editor
                        .as_mut()
                        .filter(|editor| editor.generation == generation)
                    {
                        editor.sign_in.integration = Some(integration);
                        cx.notify();
                    }
                });
                return;
            }
        }));
    }

    /// "Sign in again" or "Authorize": a fresh sign-in that starts at once
    /// when the integration has one method.
    fn request_sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_sign_in_upstream();
        let Some(editor) = &mut self.editor else {
            return;
        };
        editor.sign_in.requested = true;
        editor.sign_in.phase = SignInPhase::Idle;
        let single = editor
            .sign_in
            .integration
            .as_ref()
            .is_some_and(|integration| integration.methods.len() == 1);
        if single {
            self.start_sign_in(0, window, cx);
        }
        cx.notify();
    }

    /// Starts the sign-in with the integration's method `index`
    /// (`runConnect`): opens the browser, then follows the attempt.
    fn start_sign_in(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, target, directory)) = self.live_subject() else {
            return;
        };
        let Some(editor) = &mut self.editor else {
            return;
        };
        let Some(integration) = editor.sign_in.integration.clone() else {
            return;
        };
        let Some(method) = integration.methods.get(index).cloned() else {
            return;
        };
        if matches!(
            editor.sign_in.phase,
            SignInPhase::Starting | SignInPhase::Waiting(_) | SignInPhase::AwaitingCode { .. }
        ) {
            return;
        }
        let generation = editor.generation;
        editor.sign_in.phase = SignInPhase::Starting;
        let network = target.network.clone();
        editor.sign_in.flow = Some(cx.spawn_in(window, async move |this, cx| {
            let (client, read_directory) = (target.client.clone(), directory.clone());
            let (integration_id, method_id) = (integration.id.clone(), method.id.clone());
            let started = NetworkTask::spawn_on(&network, async move {
                client
                    .start_mcp_oauth(&integration_id, &method_id, &read_directory)
                    .await
            })
            .await;
            let settle = |this: &mut Self, phase: SignInPhase, cx: &mut Context<Self>| -> bool {
                if !this.current(&target, &directory, cx) {
                    return false;
                }
                let Some(editor) = this
                    .editor
                    .as_mut()
                    .filter(|editor| editor.generation == generation)
                else {
                    return false;
                };
                editor.sign_in.phase = phase;
                cx.notify();
                true
            };
            let attempt = match started {
                Some(Ok(attempt)) => attempt,
                other => {
                    let _ = this.update(cx, |this, cx| {
                        let text = other
                            .and_then(Result::err)
                            .and_then(|error| error.message().map(str::to_owned))
                            .map(SharedString::from)
                            .unwrap_or_else(|| {
                                locale::strings(cx)
                                    .get("settings.providers.page.toast.oauthStartFailed")
                            });
                        settle(this, SignInPhase::Failed(text), cx);
                    });
                    return;
                }
            };
            let next = match attempt.mode {
                OAuthMode::Code => SignInPhase::AwaitingCode {
                    attempt: attempt.clone(),
                    submitting: false,
                },
                OAuthMode::Auto => SignInPhase::Waiting(attempt.clone()),
            };
            let live = this
                .update(cx, |this, cx| {
                    if !settle(this, next, cx) {
                        return false;
                    }
                    if !attempt.url.is_empty() {
                        cx.open_url(&attempt.url);
                    }
                    true
                })
                .unwrap_or_default();
            if !live || attempt.mode == OAuthMode::Code {
                return;
            }
            loop {
                cx.background_executor().timer(SIGN_IN_POLL_INTERVAL).await;
                let (client, read_directory) = (target.client.clone(), directory.clone());
                let (integration_id, attempt_id) = (integration.id.clone(), attempt.id.clone());
                let progress = NetworkTask::spawn_on(&network, async move {
                    client
                        .mcp_oauth_progress(&integration_id, &attempt_id, &read_directory)
                        .await
                })
                .await;
                match progress {
                    Some(Ok(OAuthProgress::Pending)) => {}
                    Some(Ok(OAuthProgress::Complete)) => {
                        let _ = this.update_in(cx, |this, window, cx| {
                            if settle(this, SignInPhase::Idle, cx) {
                                this.signed_in(window, cx);
                            }
                        });
                        return;
                    }
                    Some(Ok(OAuthProgress::Failed(message))) => {
                        let _ = this.update(cx, |this, cx| {
                            settle(this, SignInPhase::Failed(message.into()), cx);
                        });
                        return;
                    }
                    _ => {
                        let _ = this.update(cx, |this, cx| {
                            let text = locale::strings(cx)
                                .get("settings.providers.page.toast.oauthCompleteFailed");
                            settle(this, SignInPhase::Failed(text), cx);
                        });
                        return;
                    }
                }
            }
        }));
        cx.notify();
    }

    /// Hands the pasted code to the sign-in (`submitCode`).
    fn submit_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, target, directory)) = self.live_subject() else {
            return;
        };
        let Some(editor) = &mut self.editor else {
            return;
        };
        let code = editor.sign_in.code.read(cx).value().trim().to_owned();
        let SignInPhase::AwaitingCode {
            attempt,
            submitting: false,
        } = editor.sign_in.phase.clone()
        else {
            return;
        };
        let Some(integration) = editor.sign_in.integration.clone() else {
            return;
        };
        if code.is_empty() {
            return;
        }
        editor.sign_in.phase = SignInPhase::AwaitingCode {
            attempt: attempt.clone(),
            submitting: true,
        };
        let generation = editor.generation;
        let client = target.client.clone();
        let read_directory = directory.clone();
        let work = NetworkTask::spawn_on(&target.network, async move {
            client
                .complete_mcp_oauth(&integration.id, &attempt.id, &code, &read_directory)
                .await
        });
        editor.sign_in.flow = Some(cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.current(&target, &directory, cx) {
                    return;
                }
                let Some(editor) = this
                    .editor
                    .as_mut()
                    .filter(|editor| editor.generation == generation)
                else {
                    return;
                };
                if matches!(result, Some(Ok(()))) {
                    editor.sign_in.phase = SignInPhase::Idle;
                    this.signed_in(window, cx);
                } else {
                    let text = result
                        .and_then(Result::err)
                        .and_then(|error| error.message().map(str::to_owned))
                        .map(SharedString::from)
                        .unwrap_or_else(|| {
                            locale::strings(cx)
                                .get("settings.providers.page.toast.oauthCompleteFailed")
                        });
                    editor.sign_in.phase = SignInPhase::Failed(text);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Cancel in the sign-in panel: the attempt is dropped upstream.
    fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        self.cancel_sign_in_upstream();
        if let Some(editor) = &mut self.editor {
            editor.sign_in.phase = SignInPhase::Idle;
            editor.sign_in.flow = None;
        }
        cx.notify();
    }

    /// OpenCode stored the credential; the server stays unauthorized until
    /// it is connected again with it (`handleOAuthConnected`).
    fn signed_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let strings = locale::strings(cx).clone();
        let text = strings.get("settings.providers.page.toast.oauthCompleted");
        self.toast(text, true, window, cx);
        if let Some(editor) = &mut self.editor {
            editor.sign_in.requested = false;
        }
        let Some((name, target, directory)) = self.live_subject() else {
            return;
        };
        let ticket = self.ticket(cx);
        let client = target.client.clone();
        let (server, read_directory) = (name.clone(), directory.clone());
        let work = NetworkTask::spawn_on(&target.network, async move {
            let connected = client
                .connect_mcp_server(&server, &read_directory)
                .await
                .is_ok();
            (
                connected,
                client.mcp_server_reports(&read_directory).await.ok(),
            )
        });
        self.busy = Some(Busy::Connecting);
        self.action = Some(cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.current(&target, &directory, cx) {
                    return;
                }
                this.busy = None;
                let Some((connected, reports)) = result else {
                    return;
                };
                if !connected {
                    this.connect_failures.insert(name.clone());
                }
                this.land_reports(ticket, reports, cx);
                let strings = locale::strings(cx).clone();
                let (key, succeeded) = if connected {
                    ("settings.mcp.page.toast.connected", true)
                } else {
                    ("settings.mcp.page.toast.connectionTestFailed", false)
                };
                this.toast(strings.get(key), succeeded, window, cx);
                cx.notify();
            });
        }));
    }

    /// Commits text still held by a focused field before writers freeze. It
    /// is accepted even while an earlier write is in flight.
    pub(crate) fn prepare_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.request_save(window, cx);
    }

    pub(crate) fn take_pending_write(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let pending = self.writes.take_pending_write();
        cx.notify();
        pending
    }

    pub(crate) fn has_failed_writes(&self) -> bool {
        self.save_failed
    }

    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.writes.cancel_close();
        cx.notify();
    }

    pub(crate) fn take_quit_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.writes.take_quit_write(self.network.take(), cx)
    }

    /// `mcp.create` starts a draft; the other results focus their control in
    /// the open editor.
    pub(crate) fn reveal(&mut self, item: &str, window: &mut Window, cx: &mut Context<Self>) {
        if item == "mcp.create" {
            self.create_new(window, cx);
        }
        let Some(editor) = &mut self.editor else {
            return;
        };
        match item {
            "mcp.create" | "mcp.server" if editor.saved_name.is_none() => {
                let name = editor.name.clone();
                name.update(cx, |input, cx| input.focus(window, cx));
            }
            "mcp.command" if editor.kind == McpKind::Local => {
                let command = editor.command.clone();
                command.update(cx, |input, cx| input.focus(window, cx));
            }
            "mcp.command" => {
                let url = editor.url.clone();
                url.update(cx, |input, cx| input.focus(window, cx));
            }
            "mcp.advanced" => {
                editor.advanced_open = true;
                cx.notify();
            }
            _ => {}
        }
    }
}

impl McpPage {
    fn status_pill(
        &self,
        id: SharedString,
        standing: Option<Standing>,
        enabled: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let strings = locale::strings(cx);
        let palette = cx.global::<Palette>();
        let (key, color) = match (standing, enabled) {
            (Some(Standing::Blocked), _) => (
                "settings.mcp.grid.status.blockedByPolicy",
                palette.status_warning,
            ),
            (_, false) => (
                "settings.mcp.grid.status.disabled",
                palette.muted_foreground,
            ),
            (Some(Standing::Runtime(McpConnection::Connected)), _) => (
                "settings.mcp.page.status.label.connected",
                palette.status_success,
            ),
            (Some(Standing::Runtime(McpConnection::Failed) | Standing::ConnectFailed), _) => {
                ("settings.mcp.page.status.label.failed", palette.status_error)
            }
            (Some(Standing::Runtime(McpConnection::NeedsAuth)), _) => (
                "settings.mcp.page.status.label.needsAuth",
                palette.status_warning,
            ),
            (Some(Standing::Runtime(McpConnection::Pending)), _) => {
                ("settings.mcp.grid.status.pending", palette.muted_foreground)
            }
            _ => return None,
        };
        Some(pill(id, strings.get(key), color, cx))
    }

    #[allow(clippy::too_many_arguments)]
    fn card(
        &self,
        id: SharedString,
        icon: &'static str,
        title: SharedString,
        subtitle: Option<SharedString>,
        footer: Vec<SharedString>,
        badge: Option<AnyElement>,
        muted: bool,
        cx: &Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let palette = cx.global::<Palette>().clone();
        let direction = locale::strings(cx).direction();
        div()
            .id(id)
            .w(px(260.0))
            .min_h(px(92.0))
            .p(px(12.0))
            .rounded(px(8.0))
            .bg(palette.elevated)
            .flex()
            .flex_col()
            .gap(px(6.0))
            .when(muted, |card| card.opacity(0.6))
            .child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .child(glyph(icon, 14.0, palette.muted_foreground))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(title),
                    )
                    .children(badge),
            )
            .children(subtitle.map(|subtitle| {
                div()
                    .truncate()
                    .font_family(Theme::global(cx).mono_font_family.clone())
                    .css_text(0.75, 1.0, cx)
                    .text_color(palette.muted_foreground)
                    .child(subtitle)
            }))
            .child(div().flex_1())
            .child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .css_text(0.6875, 1.0, cx)
                    .text_color(palette.muted_foreground)
                    .children(footer),
            )
    }

    /// The server list: one card per configured server with its status, then
    /// the servers OpenCode runs that no config file defines.
    fn grid(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let search = match &self.search {
            Some(search) => search.clone(),
            None => {
                let placeholder = strings.get("settings.mcp.grid.searchPlaceholder");
                let search = cx.new(|cx| {
                    let mut state = InputState::new(window, cx);
                    state.set_placeholder(placeholder, window, cx);
                    state
                });
                self._subscriptions.push(cx.subscribe_in(
                    &search,
                    window,
                    |_, _, _: &InputEvent, _, cx| cx.notify(),
                ));
                self.search = Some(search.clone());
                search
            }
        };
        let typed = search.read(cx).value().trim().to_owned();
        let query = typed.to_lowercase();
        let refresh = icon_button(
            "mcp-refresh-status".into(),
            "refresh",
            strings.get("settings.mcp.sidebar.actions.refreshStatusAria"),
            &mut self.focus,
            cx,
            |this, window, cx| this.refresh_status(window, cx),
        );
        let mut frame = page_frame("mcp-settings-page", Page::Mcp, &self.scroll, cx).child(
            div()
                .mb(px(16.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .child(
                    div().w(px(256.0)).child(
                        Input::new(&search)
                            .id("mcp-search")
                            .accessibility_id("mcp-search")
                            .aria_label(strings.get("settings.mcp.grid.searchPlaceholder")),
                    ),
                )
                .child(refresh),
        );
        let servers = match &self.list {
            Load::Failed => {
                return frame
                    .child(load_failed(
                        "mcp-load-failed",
                        strings.get("settings.openchamber.desktopNetwork.error.loadFailed"),
                        strings.get("desktopHostSwitcher.actions.retry"),
                        cx,
                        |this, cx| this.refresh(cx),
                    ))
                    .into_any_element();
            }
            Load::Loading => {
                return frame
                    .child(helper(strings.get("common.loading"), cx))
                    .into_any_element();
            }
            Load::Ready(servers) => servers.clone(),
        };
        let runtime_only = self.runtime_only(cx);
        let matches = |name: &str, target: &str| {
            query.is_empty()
                || name.to_lowercase().contains(&query)
                || target.to_lowercase().contains(&query)
        };
        let shown_servers: Vec<&McpServerConfig> = servers
            .iter()
            .filter(|server| {
                let target = match server.kind {
                    McpKind::Local => server.command.join(" "),
                    McpKind::Remote => server.url.clone(),
                };
                matches(&server.name, &target)
            })
            .collect();
        let shown_runtime: Vec<&(String, McpConnection)> = runtime_only
            .iter()
            .filter(|(name, _)| matches(name, ""))
            .collect();
        if servers.is_empty() && runtime_only.is_empty() {
            frame = frame.child(
                div()
                    .id("mcp-empty")
                    .py(px(24.0))
                    .child(helper(strings.get("settings.mcp.grid.empty"), cx)),
            );
        } else if shown_servers.is_empty() && shown_runtime.is_empty() {
            frame = frame.child(div().id("mcp-no-matches").py(px(24.0)).child(helper(
                strings.format("settings.mcp.grid.noMatches", &[("query", &typed)]),
                cx,
            )));
        }
        let mut cards = div().flex_row_from(direction).flex_wrap().gap(px(12.0));
        if query.is_empty() {
            let label = strings.get("settings.mcp.sidebar.actions.addServerTitle");
            let focus = focus_handle(&mut self.focus, &"mcp-create".into(), cx);
            let hover = palette.hover;
            cards = cards.child(
                on_activate(
                    div()
                        .id("mcp-create")
                        .accessibility_id("mcp-create")
                        .role(Role::Button)
                        .aria_label(label.clone())
                        .track_focus(&focus)
                        .tab_index(0)
                        .w(px(260.0))
                        .min_h(px(92.0))
                        .p(px(12.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_dashed()
                        .border_color(palette.border)
                        .cursor_pointer()
                        .hover(move |style| style.bg(hover))
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(
                            div()
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(6.0))
                                .child(glyph("add", 14.0, palette.muted_foreground))
                                .child(div().font_weight(FontWeight::MEDIUM).child(label)),
                        )
                        .child(helper(strings.get("settings.mcp.grid.addHint"), cx)),
                    cx,
                    |this, window, cx| this.create_new(window, cx),
                )
                .test_support(),
            );
        }
        for server in shown_servers {
            let name = server.name.clone();
            let standing = self.standing(&name, cx);
            let enabled = !server.disabled;
            let badge = self.status_pill(
                format!("mcp-status-{name}").into(),
                standing,
                enabled,
                cx,
            );
            let target = match server.kind {
                McpKind::Local => server.command.join(" "),
                McpKind::Remote => server.url.clone(),
            };
            let mut footer = vec![strings.get(match server.kind {
                McpKind::Local => "settings.mcp.sidebar.serverType.localTitle",
                McpKind::Remote => "settings.mcp.sidebar.serverType.remoteTitle",
            })];
            if server.scope == Some(McpScope::Project) {
                footer.push(strings.get("settings.mcp.grid.scope.project"));
            }
            let open_id: SharedString = format!("mcp-open-{name}").into();
            let focus = focus_handle(&mut self.focus, &open_id, cx);
            let removed = name.clone();
            let remove = icon_button(
                format!("mcp-remove-{name}").into(),
                "delete-bin",
                strings.format("settings.mcp.grid.actionsAria", &[("name", &name)]),
                &mut self.focus,
                cx,
                move |this, window, cx| this.ask_delete(removed.clone(), window, cx),
            );
            let card = self.card(
                format!("mcp-card-{name}").into(),
                match server.kind {
                    McpKind::Local => "server",
                    McpKind::Remote => "global",
                },
                name.clone().into(),
                (!target.is_empty()).then(|| target.into()),
                footer,
                badge,
                !enabled || standing == Some(Standing::Blocked),
                cx,
            );
            let hover = palette.hover;
            let opened = name.clone();
            cards = cards.child(
                div()
                    .relative()
                    .child(
                        on_activate(
                            card.accessibility_id(open_id)
                                .role(Role::Button)
                                .aria_label(SharedString::from(name))
                                .track_focus(&focus)
                                .tab_index(0)
                                .cursor_pointer()
                                .hover(move |style| style.bg(hover)),
                            cx,
                            move |this, window, cx| this.select(&opened, window, cx),
                        )
                        .test_support(),
                    )
                    // Its own hit area: a press on Delete does not open the card.
                    .child(
                        div()
                            .absolute()
                            .occlude()
                            .bottom(px(8.0))
                            .when(direction == Direction::LeftToRight, |slot| {
                                slot.right(px(8.0))
                            })
                            .when(direction == Direction::RightToLeft, |slot| {
                                slot.left(px(8.0))
                            })
                            .child(remove),
                    ),
            );
        }
        for (name, connection) in shown_runtime {
            let blocked = self.is_blocked(name);
            let disabled = *connection == McpConnection::Disabled;
            let standing = if blocked {
                Standing::Blocked
            } else {
                Standing::Runtime(*connection)
            };
            let badge = self.status_pill(
                format!("mcp-status-{name}").into(),
                Some(standing),
                !disabled,
                cx,
            );
            // No config entry backs this server, so it opens nothing and
            // offers no delete.
            let card = self.card(
                format!("mcp-runtime-{name}").into(),
                "server",
                name.clone().into(),
                None,
                vec![strings.get("settings.mcp.grid.runtimeProvided")],
                badge,
                disabled || blocked,
                cx,
            );
            cards = cards.child(card);
        }
        frame.child(cards).into_any_element()
    }

    fn section(&self, title: &'static str, first: bool, cx: &App) -> gpui_kit::Div {
        let palette = cx.global::<Palette>();
        div()
            .when(!first, |section| {
                section
                    .mt(px(32.0))
                    .pt(px(24.0))
                    .border_t_1()
                    .border_color(palette.border)
            })
            .child(
                div()
                    .id(title)
                    .role(Role::Heading)
                    .font_weight(FontWeight::SEMIBOLD)
                    .mb(px(12.0))
                    .child(locale::strings(cx).get(title)),
            )
    }

    /// The key and value rows of the environment or the request headers.
    fn pairs_editor(&mut self, kind: Pairs, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let slug = kind.slug();
        let rows: Vec<(Entity<InputState>, Entity<InputState>)> = self
            .editor
            .as_ref()
            .map(|editor| {
                editor
                    .rows(kind)
                    .iter()
                    .map(|row| (row.key.clone(), row.value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let sensitive = rows
            .iter()
            .any(|(_, value)| !value.read(cx).value().is_empty());
        let (paste_label, add_label) = match kind {
            Pairs::Environment => (
                "settings.mcp.page.env.pasteEnv",
                "settings.mcp.page.env.addVariable",
            ),
            Pairs::Headers => (
                "settings.mcp.page.advanced.pasteHeaders",
                "settings.mcp.page.env.addVariable",
            ),
        };
        let paste = text_button(
            format!("mcp-{slug}-paste"),
            strings.get(paste_label),
            true,
            cx,
            move |this, window, cx| this.paste_pairs(kind, window, cx),
        );
        let add = text_button(
            format!("mcp-{slug}-add"),
            strings.get(add_label),
            true,
            cx,
            move |this, window, cx| this.add_pair(kind, "", "", window, cx),
        );
        let mut column = div().flex().flex_col().gap(px(6.0)).child(
            div()
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .css_text(0.6875, 1.0, cx)
                .text_color(palette.muted_foreground)
                .child(
                    div()
                        .w(px(180.0))
                        .child(strings.get("settings.mcp.page.env.key")),
                )
                .child(
                    div()
                        .flex_1()
                        .child(strings.get("settings.mcp.page.env.value")),
                )
                .child(paste),
        );
        for (index, (key, value)) in rows.into_iter().enumerate() {
            let remove = icon_button(
                format!("mcp-{slug}-remove-{index}").into(),
                "delete-bin",
                strings.get("settings.mcp.page.env.removeVariableAria"),
                &mut self.focus,
                cx,
                move |this, window, cx| this.remove_pair(kind, index, window, cx),
            );
            column = column.child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div().w(px(180.0)).child(
                            Input::new(&key)
                                .id(SharedString::from(format!("mcp-{slug}-key-{index}")))
                                .accessibility_id(format!("mcp-{slug}-key-{index}"))
                                .aria_label(strings.get("settings.mcp.page.env.key")),
                        ),
                    )
                    .child(
                        div().flex_1().min_w_0().max_w(px(420.0)).child(
                            Input::new(&value)
                                .id(SharedString::from(format!("mcp-{slug}-value-{index}")))
                                .accessibility_id(format!("mcp-{slug}-value-{index}"))
                                .aria_label(strings.get("settings.mcp.page.env.value"))
                                .mask_toggle(),
                        ),
                    )
                    .child(remove),
            );
        }
        column = column.child(div().flex_row_from(direction).child(add));
        if sensitive {
            // A warning about where secrets land stays visible.
            column = column.child(helper(
                strings.get("settings.mcp.page.env.plainTextWarning"),
                cx,
            ));
        }
        column.into_any_element()
    }

    /// The progress of a started sign-in (`ProviderOAuthMethods`).
    fn sign_in_panel(&mut self, standing: Option<Standing>, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let Some(editor) = &self.editor else {
            return div().into_any_element();
        };
        let (phase, code) = (editor.sign_in.phase.clone(), editor.sign_in.code.clone());
        let methods = editor
            .sign_in
            .integration
            .as_ref()
            .map(|integration| integration.methods.clone())
            .unwrap_or_default();
        let busy = matches!(
            phase,
            SignInPhase::Starting
                | SignInPhase::Waiting(_)
                | SignInPhase::AwaitingCode {
                    submitting: true,
                    ..
                }
        );
        // A sign-in the header started with one method shows only progress.
        let offers_start = !editor.sign_in.requested || methods.len() > 1;
        let needs_auth = standing == Some(Standing::Runtime(McpConnection::NeedsAuth));
        let mut panel = div()
            .id("mcp-sign-in")
            .mt(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0));
        if offers_start || needs_auth {
            for (index, method) in methods.iter().enumerate() {
                let start = text_button(
                    format!("mcp-sign-in-start-{index}"),
                    strings.get("settings.providers.page.actions.connect"),
                    !busy,
                    cx,
                    move |this, window, cx| this.start_sign_in(index, window, cx),
                );
                panel = panel.child(
                    div()
                        .flex_row_from(direction)
                        .items_center()
                        .justify_between()
                        .gap(px(8.0))
                        .children(method.label.clone().map(SharedString::from))
                        .child(start),
                );
            }
        }
        let attempt_details = |attempt: &OAuthAttempt, cx: &Context<Self>| {
            let mut details = div().flex().flex_col().gap(px(6.0));
            if !attempt.instructions.is_empty() {
                details = details.child(helper(attempt.instructions.clone().into(), cx));
            }
            if !attempt.url.is_empty() {
                details = details.child(
                    div()
                        .truncate()
                        .css_text(0.75, 1.0, cx)
                        .text_color(palette.muted_foreground)
                        .child(SharedString::from(attempt.url.clone())),
                );
            }
            details
        };
        match phase {
            SignInPhase::Idle => {}
            SignInPhase::Starting => {
                panel = panel.child(helper(
                    strings.get("settings.providers.page.auth.oauth.starting"),
                    cx,
                ));
            }
            SignInPhase::Waiting(attempt) => {
                let url = attempt.url.clone();
                let open = text_button(
                    "mcp-sign-in-open",
                    strings.get("settings.providers.page.actions.open"),
                    !url.is_empty(),
                    cx,
                    move |_, _, cx| cx.open_url(&url),
                );
                let cancel = text_button(
                    "mcp-sign-in-cancel",
                    strings.get("settings.providers.page.actions.cancel"),
                    true,
                    cx,
                    |this, _, cx| this.cancel_sign_in(cx),
                );
                panel = panel
                    .child(attempt_details(&attempt, cx))
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("mcp-sign-in-waiting")
                                    .flex_1()
                                    .child(helper(
                                        strings.get("settings.providers.page.auth.oauth.waiting"),
                                        cx,
                                    )),
                            )
                            .child(open)
                            .child(cancel),
                    )
                    .child(helper(
                        strings.get("settings.providers.page.auth.oauth.waitingHint"),
                        cx,
                    ));
            }
            SignInPhase::AwaitingCode {
                attempt,
                submitting,
            } => {
                let filled = !code.read(cx).value().trim().is_empty();
                let complete = text_button(
                    "mcp-sign-in-complete",
                    strings.get(if submitting {
                        "settings.providers.page.actions.saving"
                    } else {
                        "settings.providers.page.actions.complete"
                    }),
                    !submitting && filled,
                    cx,
                    |this, window, cx| this.submit_code(window, cx),
                );
                let cancel = text_button(
                    "mcp-sign-in-cancel",
                    strings.get("settings.providers.page.actions.cancel"),
                    !submitting,
                    cx,
                    |this, _, cx| this.cancel_sign_in(cx),
                );
                panel = panel
                    .child(attempt_details(&attempt, cx))
                    .child(helper(
                        strings.get("settings.providers.page.auth.oauth.codeHint"),
                        cx,
                    ))
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div().flex_1().max_w(px(320.0)).child(
                                    Input::new(&code)
                                        .id("mcp-sign-in-code")
                                        .accessibility_id("mcp-sign-in-code")
                                        .disabled(submitting)
                                        .aria_label(strings.get(
                                            "settings.providers.page.auth.pasteAuthorizationCodePlaceholder",
                                        )),
                                ),
                            )
                            .child(complete)
                            .child(cancel),
                    );
            }
            SignInPhase::Failed(message) => {
                panel = panel.child(
                    div()
                        .id("mcp-sign-in-failed")
                        .accessibility_id("mcp-sign-in-failed")
                        .role(Role::Status)
                        .aria_label(message.clone())
                        .css_text(0.8125, 1.25, cx)
                        .text_color(palette.status_error)
                        .child(message),
                );
            }
        }
        panel.into_any_element()
    }

    fn editor_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let Some(form) = self.form(cx) else {
            return div().into_any_element();
        };
        let editor = self.editor.as_ref().expect("a form has an editor");
        let saved_name = editor.saved_name.clone();
        let is_new = saved_name.is_none();
        let (creating, legacy, enabled, kind) =
            (editor.creating, editor.legacy, editor.enabled, editor.kind);
        let advanced_open = editor.advanced_open;
        let scope = editor.scope;
        let name_text = editor.name.read(cx).value().to_string();
        let (name, command, url) = (
            editor.name.clone(),
            editor.command.clone(),
            editor.url.clone(),
        );
        let (timeout_startup, timeout_catalog, timeout_execution, metadata_url) = (
            editor.timeout_startup.clone(),
            editor.timeout_catalog.clone(),
            editor.timeout_execution.clone(),
            editor.metadata_url.clone(),
        );
        let (scope_picker, protocol_picker, codemode_picker) = (
            editor.scope_picker.clone(),
            editor.protocol_picker.clone(),
            editor.codemode_picker.clone(),
        );
        let has_integration = editor.sign_in.integration.is_some();
        let sign_in_requested = editor.sign_in.requested;
        let environment_count = editor.environment.len();
        let header_count = editor.headers.len();
        let closing = self.writes.closing();
        let save_failed = self.save_failed && !is_new;
        let standing = saved_name
            .as_deref()
            .and_then(|name| self.standing(name, cx));
        let blocked = standing == Some(Standing::Blocked);
        let connected = standing == Some(Standing::Runtime(McpConnection::Connected));
        let needs_auth = standing == Some(Standing::Runtime(McpConnection::NeedsAuth));
        let failed = matches!(
            standing,
            Some(Standing::Runtime(McpConnection::Failed) | Standing::ConnectFailed)
        );
        let reason = saved_name
            .as_deref()
            .filter(|_| failed || needs_auth)
            .and_then(|name| self.reasons.get(name).cloned());
        let offers_sign_in_again = failed && has_integration;
        let offers_authorize =
            has_integration && !offers_sign_in_again && !needs_auth && !blocked;
        let busy = self.busy;
        let title: SharedString = match &saved_name {
            Some(name) => name.clone().into(),
            None => strings.get("settings.mcp.page.header.newServer"),
        };
        let subtitle = if is_new {
            strings.get("settings.mcp.page.header.configureNewServer")
        } else {
            let transport = strings.get(match kind {
                McpKind::Local => "settings.mcp.page.transport.local",
                McpKind::Remote => "settings.mcp.page.transport.remote",
            });
            strings.format("settings.mcp.page.header.transport", &[("type", &transport)])
        };
        let back = icon_button(
            "mcp-back".into(),
            "arrow-left",
            strings.get("settings.mcp.page.back"),
            &mut self.focus,
            cx,
            |this, window, cx| this.back(window, cx),
        );
        let header_pill = (!is_new)
            .then(|| self.status_pill("mcp-editor-status".into(), standing, enabled, cx))
            .flatten();
        let mut actions = div()
            .flex_row_from(direction)
            .flex_wrap()
            .items_center()
            .gap(px(8.0));
        if !is_new {
            if offers_sign_in_again {
                actions = actions.child(text_button(
                    "mcp-sign-in-again",
                    strings.get("settings.mcp.page.actions.signInAgain"),
                    enabled,
                    cx,
                    |this, window, cx| this.request_sign_in(window, cx),
                ));
            } else {
                let label = if busy == Some(Busy::Connecting) {
                    "settings.mcp.page.actions.working"
                } else if connected {
                    "settings.mcp.page.actions.disconnect"
                } else {
                    "settings.mcp.page.actions.connect"
                };
                actions = actions.child(text_button(
                    "mcp-toggle-connect",
                    strings.get(label),
                    busy.is_none() && enabled && !blocked,
                    cx,
                    |this, window, cx| this.toggle_connect(window, cx),
                ));
            }
            if offers_authorize {
                actions = actions.child(text_button(
                    "mcp-authorize",
                    strings.get("settings.mcp.page.actions.authorize"),
                    enabled,
                    cx,
                    |this, window, cx| this.request_sign_in(window, cx),
                ));
            }
            if connected {
                let label = if busy == Some(Busy::Testing) {
                    "settings.mcp.page.actions.testing"
                } else {
                    "settings.mcp.page.actions.test"
                };
                actions = actions.child(text_button(
                    "mcp-test",
                    strings.get(label),
                    busy.is_none() && enabled,
                    cx,
                    |this, window, cx| this.test_connection(window, cx),
                ));
            }
        }
        // Only error and warning states get the full card; a started
        // sign-in needs it too.
        let status_card = (!is_new && (failed || needs_auth || blocked || sign_in_requested))
            .then(|| {
                let description: SharedString = match standing {
                    Some(Standing::Blocked) => {
                        strings.get("settings.mcp.page.status.description.blockedByPolicy")
                    }
                    Some(Standing::Runtime(McpConnection::Connected)) => {
                        strings.get("settings.mcp.page.status.description.connected")
                    }
                    Some(Standing::Runtime(McpConnection::Disabled)) => {
                        strings.get("settings.mcp.page.status.description.disabled")
                    }
                    _ if failed => reason.clone().map(SharedString::from).unwrap_or_else(|| {
                        strings.get("settings.mcp.page.status.description.failedDefault")
                    }),
                    _ if needs_auth => reason.clone().map(SharedString::from).unwrap_or_else(
                        || strings.get("settings.mcp.page.status.description.needsAuth"),
                    ),
                    _ => strings.get("settings.mcp.page.status.description.default"),
                };
                let scope_line = match scope {
                    McpScope::Project => {
                        let directory = self.directory.clone().unwrap_or_else(|| {
                            strings
                                .get("settings.mcp.page.status.activeProject")
                                .to_string()
                        });
                        strings.format(
                            "settings.mcp.page.status.projectScopedTo",
                            &[("directory", &directory)],
                        )
                    }
                    McpScope::User => strings.get("settings.mcp.page.status.userScoped"),
                };
                let test = (!connected).then(|| {
                    let label = if busy == Some(Busy::Testing) {
                        "settings.mcp.page.actions.testing"
                    } else {
                        "settings.mcp.page.actions.testConnection"
                    };
                    text_button(
                        "mcp-test-connection",
                        strings.get(label),
                        busy.is_none() && enabled && !blocked,
                        cx,
                        |this, window, cx| this.test_connection(window, cx),
                    )
                });
                let card_pill =
                    self.status_pill("mcp-card-status".into(), standing, enabled, cx);
                let sign_in = (needs_auth || sign_in_requested)
                    .then(|| self.sign_in_panel(standing, cx));
                div()
                    .id("mcp-status-card")
                    .mb(px(24.0))
                    .p(px(12.0))
                    .rounded(px(8.0))
                    // The fill already says warning or error.
                    .bg(if failed {
                        palette.status_error_background
                    } else if needs_auth || blocked {
                        palette.status_warning_background
                    } else {
                        palette.elevated
                    })
                    .child(
                        div()
                            .flex_row_from(direction)
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.0))
                            .child(strings.get("settings.mcp.page.status.runtimeStatus"))
                            .children(card_pill)
                            .child(div().flex_1())
                            .children(test),
                    )
                    .child(
                        div()
                            .id("mcp-status-description")
                            .mt(px(4.0))
                            .css_text(0.8125, 1.25, cx)
                            .text_color(palette.muted_foreground)
                            .child(description),
                    )
                    .child(
                        div()
                            .css_text(0.6875, 1.0, cx)
                            .text_color(palette.muted_foreground)
                            .child(scope_line),
                    )
                    .children(sign_in)
            });
        let enable_focus = focus_handle(&mut self.focus, &"mcp-enabled".into(), cx);
        let mut server = self.section("settings.mcp.page.server.title", true, cx);
        if is_new {
            server = server.child(field_row(
                "mcp.server",
                strings.get("settings.mcp.page.server.name"),
                None,
                div()
                    .flex_row_from(direction)
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div().w(px(192.0)).child(
                            Input::new(&name)
                                .id("mcp-name")
                                .accessibility_id("mcp-name")
                                .aria_label(strings.get("settings.mcp.page.server.name")),
                        ),
                    )
                    .child(
                        div().w(px(160.0)).key_context("SettingsPicker").child(
                            Select::new(&scope_picker)
                                .id("mcp-scope")
                                .accessibility_label(strings.get("snippets.field.scopeAria"))
                                .w_full(),
                        ),
                    )
                    .into_any_element(),
                None,
                cx,
            ));
        }
        server = server.child(checkbox_row(
            "mcp-enabled".into(),
            strings.get("settings.mcp.page.server.enable"),
            strings.get("settings.mcp.page.server.enableAria"),
            enabled,
            !closing,
            &enable_focus,
            None,
            cx,
            |this: &mut Self, checked, window, cx| this.set_enabled(checked, window, cx),
        ));
        let kinds = chip_group(
            "mcp-kind".into(),
            strings.get("settings.mcp.page.connection.title"),
            vec![
                ChipOption {
                    id: "mcp-kind-local".into(),
                    label: strings.get("settings.mcp.page.connection.kindCommand"),
                    selected: kind == McpKind::Local,
                },
                ChipOption {
                    id: "mcp-kind-remote".into(),
                    label: strings.get("settings.mcp.page.connection.kindLink"),
                    selected: kind == McpKind::Remote,
                },
            ],
            !closing,
            &mut self.focus,
            cx,
            |this, index, window, cx| {
                let kind = if index == 0 {
                    McpKind::Local
                } else {
                    McpKind::Remote
                };
                this.set_kind(kind, window, cx);
            },
        );
        let mono = Theme::global(cx).mono_font_family.clone();
        let connection = self
            .section("settings.mcp.page.connection.title", false, cx)
            .child(helper(
                strings.get("settings.mcp.page.connection.description"),
                cx,
            ))
            .child(div().mt(px(12.0)).flex_row_from(direction).child(kinds))
            .child(
                div()
                    .mt(px(12.0))
                    .css_text(0.8125, 1.25, cx)
                    .font_family(mono.clone())
                    .child(match kind {
                        McpKind::Local => Textarea::new(&command)
                            .accessibility_id("mcp-command")
                            .aria_label(strings.get("settings.mcp.page.connection.command"))
                            .into_any_element(),
                        McpKind::Remote => Input::new(&url)
                            .id("mcp-url")
                            .accessibility_id("mcp-url")
                            .aria_label(
                                strings.get("settings.mcp.page.connection.serverUrlPlaceholder"),
                            )
                            .into_any_element(),
                    }),
            )
            .children(
                (kind == McpKind::Local && !form.command.is_empty()).then(|| {
                    let count = form.command.len().to_string();
                    div().mt(px(6.0)).child(helper(
                        strings.format(
                            "settings.mcp.page.connection.previewArgs",
                            &[("count", &count)],
                        ),
                        cx,
                    ))
                }),
            )
            .child(div().mt(px(6.0)).child(helper(
                strings.get(match kind {
                    McpKind::Local => "settings.mcp.page.connection.hintCommand",
                    McpKind::Remote => "settings.mcp.page.connection.hintLink",
                }),
                cx,
            )));
        let summary = form.advanced_summary(&strings);
        let toggle_focus = focus_handle(&mut self.focus, &"mcp-advanced-toggle".into(), cx);
        let configure = strings.get("settings.mcp.page.advanced.configure");
        let mut advanced = self
            .section("settings.mcp.page.advanced.title", false, cx)
            .child(
                on_activate(
                    div()
                        .id("mcp-advanced-toggle")
                        .accessibility_id("mcp-advanced-toggle")
                        .role(Role::Button)
                        .aria_label(configure.clone())
                        .aria_expanded(advanced_open)
                        .track_focus(&toggle_focus)
                        .tab_index(0)
                        .cursor_pointer()
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(6.0))
                        .child(configure)
                        .children((!summary.is_empty()).then(|| {
                            div()
                                .css_text(0.6875, 1.0, cx)
                                .text_color(palette.muted_foreground)
                                .child(SharedString::from(format!("({summary})")))
                        }))
                        .child(div().flex_1())
                        .child(glyph(
                            if advanced_open {
                                "arrow-down-s"
                            } else {
                                "arrow-right-s"
                            },
                            16.0,
                            palette.muted_foreground,
                        )),
                    cx,
                    |this, _, cx| {
                        if let Some(editor) = &mut this.editor {
                            editor.advanced_open = !editor.advanced_open;
                            cx.notify();
                        }
                    },
                )
                .test_support(),
            );
        if advanced_open {
            let picker = |id: &'static str, state: &Picker, label: SharedString| {
                div()
                    .w(px(224.0))
                    .key_context("SettingsPicker")
                    .child(
                        Select::new(state)
                            .id(id)
                            .accessibility_label(label)
                            .w_full(),
                    )
                    .into_any_element()
            };
            let number = |id: &'static str, state: &Entity<InputState>, label: SharedString| {
                div()
                    .w(px(128.0))
                    .child(
                        Input::new(state)
                            .id(id)
                            .accessibility_id(id)
                            .aria_label(label),
                    )
                    .into_any_element()
            };
            let mut rows = div().mt(px(8.0)).flex().flex_col().gap(px(6.0));
            rows = rows.child(field_row(
                "mcp.protocol",
                strings.get("settings.mcp.page.advanced.protocol"),
                Some(strings.get("settings.mcp.page.advanced.protocolHint")),
                picker(
                    "mcp-protocol",
                    &protocol_picker,
                    strings.get("settings.mcp.page.advanced.protocol"),
                ),
                None,
                cx,
            ));
            if kind == McpKind::Local {
                rows = rows.child(field_row(
                    "mcp.timeout-startup",
                    strings.get("settings.mcp.page.advanced.timeoutStartupMs"),
                    Some(strings.get("settings.mcp.page.advanced.timeoutStartupHint")),
                    number(
                        "mcp-timeout-startup",
                        &timeout_startup,
                        strings.get("settings.mcp.page.advanced.timeoutStartupMs"),
                    ),
                    None,
                    cx,
                ));
            }
            rows = rows
                .child(field_row(
                    "mcp.timeout-catalog",
                    strings.get("settings.mcp.page.advanced.timeoutCatalogMs"),
                    Some(strings.get("settings.mcp.page.advanced.timeoutCatalogHint")),
                    number(
                        "mcp-timeout-catalog",
                        &timeout_catalog,
                        strings.get("settings.mcp.page.advanced.timeoutCatalogMs"),
                    ),
                    None,
                    cx,
                ))
                .child(field_row(
                    "mcp.timeout-execution",
                    strings.get("settings.mcp.page.advanced.timeoutExecutionMs"),
                    Some(strings.get("settings.mcp.page.advanced.timeoutExecutionHint")),
                    number(
                        "mcp-timeout-execution",
                        &timeout_execution,
                        strings.get("settings.mcp.page.advanced.timeoutExecutionMs"),
                    ),
                    None,
                    cx,
                ))
                .child(field_row(
                    "mcp.codemode",
                    strings.get("settings.mcp.page.advanced.codemode"),
                    Some(
                        format!(
                            "{} {}",
                            strings.get("settings.mcp.page.advanced.codemodeHint"),
                            strings.get("settings.mcp.page.advanced.codemodeDefaultHint"),
                        )
                        .into(),
                    ),
                    picker(
                        "mcp-codemode",
                        &codemode_picker,
                        strings.get("settings.mcp.page.advanced.codemode"),
                    ),
                    None,
                    cx,
                ));
            if kind == McpKind::Remote {
                let headers_title: SharedString = if header_count > 0 {
                    format!(
                        "{} ({header_count})",
                        strings.get("settings.mcp.page.advanced.requestHeaders")
                    )
                    .into()
                } else {
                    strings.get("settings.mcp.page.advanced.requestHeaders")
                };
                let invalid = !form.metadata_url.trim().is_empty()
                    && !is_absolute_http_url(form.metadata_url.trim());
                rows = rows
                    .child(
                        div()
                            .mt(px(8.0))
                            .font_weight(FontWeight::MEDIUM)
                            .child(headers_title),
                    )
                    .child(self.pairs_editor(Pairs::Headers, cx))
                    .child(
                        div()
                            .mt(px(8.0))
                            .font_weight(FontWeight::MEDIUM)
                            .child(strings.get("settings.mcp.page.advanced.oauth")),
                    )
                    .child(field_row(
                        "mcp.oauth-metadata-url",
                        strings.get("settings.mcp.page.advanced.oauthMetadataUrl"),
                        Some(strings.get("settings.mcp.page.advanced.oauthMetadataUrlHint")),
                        div()
                            .w(px(384.0))
                            .max_w_full()
                            .child(
                                Input::new(&metadata_url)
                                    .id("mcp-oauth-metadata-url")
                                    .accessibility_id("mcp-oauth-metadata-url")
                                    .aria_label(
                                        strings.get("settings.mcp.page.advanced.oauthMetadataUrl"),
                                    ),
                            )
                            .into_any_element(),
                        None,
                        cx,
                    ))
                    // A validation error stays readable while it is fixed.
                    .children(invalid.then(|| {
                        div()
                            .id("mcp-oauth-metadata-url-invalid")
                            .css_text(0.6875, 1.0, cx)
                            .text_color(palette.status_error)
                            .child(
                                strings.get("settings.mcp.page.advanced.oauthMetadataUrlInvalid"),
                            )
                    }));
            }
            advanced = advanced.child(rows);
        }
        let environment_title: SharedString = if environment_count > 0 {
            format!(
                "{} ({environment_count})",
                strings.get("settings.mcp.page.env.title")
            )
            .into()
        } else {
            strings.get("settings.mcp.page.env.title")
        };
        let environment_body = if environment_count == 0 {
            div()
                .flex_row_from(direction)
                .child(text_button(
                    "mcp-env-add-first",
                    strings.get("settings.mcp.page.env.addEnvironmentVariable"),
                    true,
                    cx,
                    |this, window, cx| this.add_pair(Pairs::Environment, "", "", window, cx),
                ))
                .into_any_element()
        } else {
            self.pairs_editor(Pairs::Environment, cx)
        };
        let environment = div()
            .mt(px(32.0))
            .pt(px(24.0))
            .border_t_1()
            .border_color(palette.border)
            .child(
                div()
                    .id("mcp-environment-title")
                    .role(Role::Heading)
                    .font_weight(FontWeight::SEMIBOLD)
                    .mb(px(4.0))
                    .child(environment_title),
            )
            .child(
                div()
                    .mb(px(12.0))
                    .child(helper(strings.get("settings.mcp.page.env.description"), cx)),
            )
            .child(environment_body);
        let mut footer = div()
            .pt(px(16.0))
            .pb(px(32.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0));
        if is_new {
            footer = footer
                .child(text_button(
                    "mcp-create-confirm",
                    strings.get(if creating {
                        "settings.common.actions.saving"
                    } else {
                        "settings.common.actions.create"
                    }),
                    !creating && !closing && !name_text.trim().is_empty(),
                    cx,
                    |this, window, cx| this.create(window, cx),
                ))
                .child(text_button(
                    "mcp-create-cancel",
                    strings.get("settings.common.actions.cancel"),
                    !creating,
                    cx,
                    |this, _, cx| this.close_editor(cx),
                ));
        } else if let Some(name) = saved_name.clone() {
            let deleting = self.deleting.contains(&name);
            footer = footer.child(text_button(
                "mcp-delete",
                strings.get(if deleting {
                    "settings.mcp.page.actions.deleting"
                } else {
                    "settings.common.actions.delete"
                }),
                !deleting && !closing,
                cx,
                move |this, window, cx| this.ask_delete(name.clone(), window, cx),
            ));
        }
        let failed_save = save_failed.then(|| {
            load_failed(
                "mcp-save-failed",
                strings.get("settings.common.status.saveFailed"),
                strings.get("desktopHostSwitcher.actions.retry"),
                cx,
                |this, cx| {
                    // The next render has the window a write completes in.
                    this.retry_requested = true;
                    cx.notify();
                },
            )
        });
        div()
            .css_text(0.875, 1.25, cx)
            .id("mcp-editor")
            .role(Role::Group)
            .aria_label(strings.get(Page::Mcp.title()))
            .size_full()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(48.0))
            .py(px(32.0))
            .text_color(palette.foreground)
            .when(direction == Direction::RightToLeft, |view| {
                view.text_right()
            })
            .child(
                div()
                    .mb(px(24.0))
                    .flex_row_from(direction)
                    .flex_wrap()
                    .items_start()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex_row_from(direction)
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(back)
                                    .child(
                                        div()
                                            .id("mcp-title")
                                            .accessibility_id("mcp-title")
                                            .role(Role::Heading)
                                            .aria_label(title.clone())
                                            .truncate()
                                            .css_text(1.125, 1.5, cx)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(title),
                                    )
                                    .children(header_pill),
                            )
                            .child(
                                div()
                                    .mt(px(4.0))
                                    .text_color(palette.muted_foreground)
                                    .child(subtitle),
                            ),
                    )
                    .child(actions),
            )
            .children((legacy && !is_new).then(|| {
                div()
                    .id("mcp-legacy-note")
                    .mb(px(16.0))
                    .flex_row_from(direction)
                    .items_start()
                    .gap(px(6.0))
                    .child(glyph("information", 14.0, palette.muted_foreground))
                    .child(helper(strings.get("settings.common.legacyFormat.note"), cx))
            }))
            .children(failed_save)
            .children(status_card)
            .child(server)
            .child(connection)
            .child(advanced)
            .child(environment)
            .child(footer)
            .into_any_element()
    }
}

impl Render for McpPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        if std::mem::take(&mut self.retry_requested) {
            self.request_save(window, cx);
        }
        if std::mem::take(&mut self.rehydrate_requested)
            && let Some(editor) = &self.editor
            && editor.submitted.is_none()
            && editor.sign_in.phase == SignInPhase::Idle
            && self.form(cx).as_ref() == Some(&editor.saved)
        {
            // The entry changed on disk under a clean form, for example the
            // work status card switched the server off.
            let advanced_open = editor.advanced_open;
            let stored = editor.saved_name.clone().and_then(|name| {
                self.servers()
                    .iter()
                    .find(|server| server.name == name)
                    .cloned()
            });
            if let Some(stored) = stored {
                self.open_editor(Some(&stored), "", window, cx);
                if let Some(editor) = &mut self.editor {
                    editor.advanced_open = advanced_open;
                }
            }
        }
        if self.target.is_none() || self.directory.is_none() {
            return div()
                .id("mcp-settings-page")
                .size_full()
                .px(px(48.0))
                .py(px(32.0))
                .child(crate::settings_view::page_header(Page::Mcp, cx))
                .child(helper(strings.get("common.unavailable"), cx))
                .into_any_element();
        }
        if self.editor.is_some() {
            self.editor_view(cx)
        } else {
            self.grid(window, cx)
        }
    }
}

#[cfg(test)]
#[path = "mcp_settings_tests.rs"]
mod tests;
