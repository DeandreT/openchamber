//! Settings → Providers (`ProvidersPage.tsx`, `ProviderGrid.tsx`,
//! `ProviderConnectPicker.tsx`, `ProviderApiKeyForm.tsx`,
//! `ProviderOAuthMethods.tsx`, `CustomProviderForm.tsx`): the providers
//! OpenCode lists with their connection state, one provider's accounts and
//! models, and the ways to connect a new one.
//!
//! OpenCode owns every credential. A key typed here lives in a masked input
//! until Save hands it to the one request that stores it; it is never logged,
//! never kept for a retry and never part of a message, and the input is
//! emptied once the server took it. A failed key write keeps the text in the
//! field so the user can press Save again. Leaving a view, hiding the page
//! and switching servers drop what the secret fields held.
//!
//! In enterprise mode providers come from the OpenCode config. The page then
//! shows who manages the app, offers no way to connect, and every connect
//! path refuses before it sends anything; the server refuses too. Removing a
//! stored account and hiding models stay available. The mode is read with the
//! page's other reads, so an unread policy fails the page instead of showing
//! it unlocked.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{Input, InputContentType, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, Toggled, WeakEntity,
    Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_transport::Url;
use openchamber_native_transport::enterprise_policy::ServerPolicy;
use openchamber_native_transport::event::CatalogKind;
use openchamber_native_transport::providers::{
    Connection, CredentialMethod, CustomProtocol, CustomProvider, Integration, ListedProvider,
    OAuthAttempt, OAuthMode, OAuthStatus, ProviderError, SecretKey, SignInKind, SignInMethod,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Strings};
use crate::logical::LogicalStyledExt;
use crate::network::{Network, NetworkTask};
use crate::settings_controls::{
    Load, ServerTarget, SettingsWrites, focus_handle, helper, info_hint, load_failed, page_frame,
    same_target, section_with_info,
};
use crate::settings_navigation::Page;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// How often an `auto` attempt is asked again while the user signs in.
const POLL_INTERVAL: Duration = Duration::from_millis(1500);
/// `KEY_METHOD_RETRY_DELAYS_MS`: OpenCode registers a custom provider's key
/// method once its file watcher saw the config write.
const KEY_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];
/// `POPULAR_PROVIDERS`: what leads the connect list, with each one's note.
const POPULAR: [(&str, &str); 7] = [
    ("openai", "settings.providers.connect.note.openai"),
    ("opencode-go", "settings.providers.connect.note.opencodeGo"),
    ("openrouter", "settings.providers.connect.note.openrouter"),
    ("anthropic", "settings.providers.connect.note.anthropic"),
    (
        "github-copilot",
        "settings.providers.connect.note.githubCopilot",
    ),
    ("google", "settings.providers.connect.note.google"),
    ("vercel", "settings.providers.connect.note.vercel"),
];

const ACCOUNTS: usize = 0;
const MODELS: usize = 1;

/// The integration whose sign-in methods sign `provider_id` in
/// (`getSignInIntegrationId`): OpenCode Go signs in through the Console.
fn sign_in_integration_id(provider_id: &str) -> &str {
    if provider_id == "opencode-go" {
        "opencode"
    } else {
        provider_id
    }
}

fn find<'a>(integrations: &'a [Integration], id: &str) -> Option<&'a Integration> {
    integrations.iter().find(|integration| integration.id == id)
}

/// The connections that give a provider credentials: its own integration's
/// and its sign-in integration's. `None` when OpenCode has neither.
fn connections<'a>(
    integrations: &'a [Integration],
    provider_id: &str,
) -> Option<Vec<&'a Connection>> {
    let own = find(integrations, provider_id);
    let sign_in_id = sign_in_integration_id(provider_id);
    let sign_in = (sign_in_id != provider_id)
        .then(|| find(integrations, sign_in_id))
        .flatten();
    if own.is_none() && sign_in.is_none() {
        return None;
    }
    Some(
        own.into_iter()
            .chain(sign_in)
            .flat_map(|integration| &integration.connections)
            .collect(),
    )
}

/// The one-glance state of a provider's row (`getProviderCardStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CardStatus {
    ReauthNeeded,
    Accounts(usize),
    Connected,
    NoModels,
    Environment,
    SignInNeeded,
}

/// A provider without an integration has nothing OpenCode can sign in to,
/// so it gets no status instead of a false "not signed in".
fn card_status(integrations: &[Integration], provider: &ListedProvider) -> Option<CardStatus> {
    let connections = connections(integrations, &provider.id);
    let listed = connections.as_deref().unwrap_or_default();
    if listed.iter().any(|connection| connection.needs_auth()) {
        return Some(CardStatus::ReauthNeeded);
    }
    if provider.models.is_empty() {
        return Some(CardStatus::NoModels);
    }
    let credentials = listed
        .iter()
        .filter(|connection| matches!(connection, Connection::Credential { .. }))
        .count();
    match credentials {
        0 if listed
            .iter()
            .any(|connection| matches!(connection, Connection::Environment { .. })) =>
        {
            Some(CardStatus::Environment)
        }
        0 => connections.map(|_| CardStatus::SignInNeeded),
        1 => Some(CardStatus::Connected),
        count => Some(CardStatus::Accounts(count)),
    }
}

/// The providers the page lists: those with a model, and one without any
/// only while it holds a stored login to manage.
fn shown_providers<'a>(
    providers: &'a [ListedProvider],
    integrations: &[Integration],
) -> Vec<&'a ListedProvider> {
    providers
        .iter()
        .filter(|provider| {
            !provider.models.is_empty()
                || find(integrations, &provider.id)
                    .is_some_and(|integration| integration.credentials().next().is_some())
        })
        .collect()
}

/// What can still be connected (`listConnectableProviders`): integrations
/// with no connection that are neither MCP sign-ins, web search accounts nor
/// providers already listed. Sorted by name.
fn connectable<'a>(
    integrations: &'a [Integration],
    listed: &HashSet<&str>,
) -> Vec<&'a Integration> {
    let mut candidates: Vec<&Integration> = integrations
        .iter()
        .filter(|integration| {
            integration.connections.is_empty()
                && !integration.id.starts_with("mcp_")
                && !integration.web_search
                && !listed.contains(integration.id.as_str())
        })
        .collect();
    candidates.sort_by_key(|integration| integration.name.to_lowercase());
    candidates
}

/// Device codes travel inside the instructions (`Enter code: ABCD-1234`), so
/// they are recovered by shape (`extractUserCode`): four letters or digits, a
/// dash, then four or five more.
fn user_code(instructions: &str) -> Option<&str> {
    let code_char = |byte: u8| byte.is_ascii_uppercase() || byte.is_ascii_digit();
    let bytes = instructions.as_bytes();
    (0..bytes.len()).find_map(|start| {
        let head = bytes.get(start..start + 4)?;
        if !head.iter().copied().all(code_char) || bytes.get(start + 4) != Some(&b'-') {
            return None;
        }
        let tail = bytes[start + 5..]
            .iter()
            .take(5)
            .take_while(|byte| code_char(**byte))
            .count();
        (tail >= 4).then(|| &instructions[start..start + 5 + tail])
    })
}

/// Whether the system browser may be handed `url`: only a web address. The
/// address comes from the server, and anything else could start a local
/// handler.
fn opens_in_browser(url: &str) -> bool {
    Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// What the custom provider's key field holds, without the key itself.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DraftCredential {
    Missing,
    /// `{env:VAR}`: the key is read from the server's environment.
    Env(String),
    /// A literal key, which travels on its own as a [`SecretKey`].
    Key,
}

/// Splits the key field (`parseEnvApiKey`).
fn draft_credential(text: &str) -> (DraftCredential, Option<SecretKey>) {
    let text = text.trim();
    if let Some(variable) = text
        .strip_prefix("{env:")
        .and_then(|rest| rest.strip_suffix('}'))
        .map(str::trim)
        .filter(|variable| !variable.is_empty() && !variable.contains('}'))
    {
        return (DraftCredential::Env(variable.to_owned()), None);
    }
    match SecretKey::new(text) {
        Some(key) => (DraftCredential::Key, Some(key)),
        None => (DraftCredential::Missing, None),
    }
}

/// The custom provider form as typed, without key material.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CustomDraft {
    id: String,
    name: String,
    base_url: String,
    protocol: CustomProtocol,
    /// `(model ID, display name)` rows.
    models: Vec<(String, String)>,
}

/// One message key per field that keeps the form from being saved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CustomErrors {
    id: Option<&'static str>,
    name: Option<&'static str>,
    base_url: Option<&'static str>,
    api_key: Option<&'static str>,
    /// Per model row: the ID's error and the name's.
    models: Vec<(Option<&'static str>, Option<&'static str>)>,
}

impl CustomErrors {
    fn any(&self) -> bool {
        self.id.is_some()
            || self.name.is_some()
            || self.base_url.is_some()
            || self.api_key.is_some()
            || self
                .models
                .iter()
                .any(|(id, name)| id.is_some() || name.is_some())
    }
}

/// Checks the form and builds the config write (`validateCustomProvider`).
/// `existing` are the providers OpenCode already lists; `persisted` is the ID
/// an earlier save of this form already wrote, which may be written again.
fn validate_custom(
    draft: &CustomDraft,
    credential: &DraftCredential,
    existing: &HashSet<&str>,
    persisted: Option<&str>,
) -> Result<CustomProvider, CustomErrors> {
    const REQUIRED: &str = "settings.providers.page.custom.error.required";
    let (id, name, base_url) = (draft.id.trim(), draft.name.trim(), draft.base_url.trim());
    let id_shape = id
        .bytes()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    let mut seen = HashSet::new();
    let errors = CustomErrors {
        id: if id.is_empty() {
            Some("settings.providers.page.custom.error.providerID.required")
        } else if !id_shape {
            Some("settings.providers.page.custom.error.providerID.format")
        } else if existing.contains(id) && persisted != Some(id) {
            Some("settings.providers.page.custom.error.providerID.exists")
        } else {
            None
        },
        name: name
            .is_empty()
            .then_some("settings.providers.page.custom.error.name.required"),
        base_url: if base_url.is_empty() {
            Some("settings.providers.page.custom.error.baseURL.required")
        } else if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            Some("settings.providers.page.custom.error.baseURL.format")
        } else {
            None
        },
        api_key: (*credential == DraftCredential::Missing)
            .then_some("settings.providers.page.custom.error.apiKey.required"),
        models: draft
            .models
            .iter()
            .map(|(model_id, model_name)| {
                let model_id = model_id.trim();
                let id_error = if model_id.is_empty() {
                    Some(REQUIRED)
                } else if !seen.insert(model_id) {
                    Some("settings.providers.page.custom.error.duplicate")
                } else {
                    None
                };
                (id_error, model_name.trim().is_empty().then_some(REQUIRED))
            })
            .collect(),
    };
    if errors.any() {
        return Err(errors);
    }
    Ok(CustomProvider {
        id: id.to_owned(),
        name: name.to_owned(),
        protocol: draft.protocol,
        base_url: base_url.to_owned(),
        env: match credential {
            DraftCredential::Env(variable) => Some(variable.clone()),
            _ => None,
        },
        models: draft
            .models
            .iter()
            .map(|(model_id, model_name)| {
                (model_id.trim().to_owned(), model_name.trim().to_owned())
            })
            .collect(),
        has_credential: *credential == DraftCredential::Key,
    })
}

/// How a custom provider's save ended.
enum CustomOutcome {
    Saved,
    /// Nothing was written.
    ConfigFailed(ProviderError),
    /// The provider is in the config and its key is not stored.
    KeyFailed(ProviderError),
}

/// Everything the page shows, from one round of reads.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Loaded {
    providers: Vec<ListedProvider>,
    integrations: Vec<Integration>,
    /// The server's enterprise policy; `None` for a server from before it.
    policy: Option<ServerPolicy>,
    /// What the model rows show: the confirmed list with a change in flight.
    hidden: Vec<(String, String)>,
    /// The hidden models the server last read or answered with.
    confirmed_hidden: Vec<(String, String)>,
}

/// Which part of the page is open.
#[derive(Clone, Debug, PartialEq, Eq)]
enum View {
    /// Every listed provider.
    List,
    /// One listed provider's accounts and models.
    Provider(String),
    /// What can still be connected.
    Connect,
    /// A provider chosen to connect.
    Candidate(String),
    /// The custom provider form.
    Custom,
}

/// A sign-in in progress. Only one runs at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Flow {
    Idle,
    /// The attempt or the external connection was asked for.
    Connecting {
        provider: String,
        method: String,
    },
    /// `auto`: polled until the browser sign-in finishes.
    Waiting {
        provider: String,
        method: String,
        attempt: OAuthAttempt,
    },
    /// `code`: waiting for the code the user copies out of the browser.
    AwaitingCode {
        provider: String,
        method: String,
        attempt: OAuthAttempt,
        submitting: bool,
    },
    Failed {
        provider: String,
        method: String,
        message: SharedString,
    },
}

impl Flow {
    fn provider(&self) -> Option<&str> {
        match self {
            Self::Idle => None,
            Self::Connecting { provider, .. }
            | Self::Waiting { provider, .. }
            | Self::AwaitingCode { provider, .. }
            | Self::Failed { provider, .. } => Some(provider),
        }
    }

    /// An attempt OpenCode still holds.
    fn attempt(&self) -> Option<(&str, &OAuthAttempt)> {
        match self {
            Self::Waiting {
                provider, attempt, ..
            }
            | Self::AwaitingCode {
                provider, attempt, ..
            } => Some((provider, attempt)),
            _ => None,
        }
    }

    fn busy(&self) -> bool {
        matches!(
            self,
            Self::Connecting { .. }
                | Self::Waiting { .. }
                | Self::AwaitingCode {
                    submitting: true,
                    ..
                }
        )
    }
}

/// A write the server did not take, kept so Retry can send it again. None
/// of them carries a key.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FailedWrite {
    /// The whole hidden list as the rows last asked for it.
    Hidden(Vec<(String, String)>),
    Remove {
        id: String,
        label: String,
    },
}

struct ModelRow {
    id: Entity<InputState>,
    name: Entity<InputState>,
}

/// The page's text fields. They are dropped whenever the page lets go of
/// its server or is hidden, which is how the secret ones are emptied.
struct Inputs {
    /// Filters the list in view.
    search: Entity<InputState>,
    /// Secret: the key of the open provider or candidate.
    key: Entity<InputState>,
    /// Secret: the authorization code of a `code` attempt.
    code: Entity<InputState>,
    custom_id: Entity<InputState>,
    custom_name: Entity<InputState>,
    custom_url: Entity<InputState>,
    /// Secret: the custom provider's key, or `{env:VAR}`.
    custom_key: Entity<InputState>,
    models: Vec<ModelRow>,
    _subscriptions: Vec<Subscription>,
}

pub(crate) struct ProvidersPage {
    owner: WeakEntity<Workspace>,
    network: Option<Network>,
    target: Option<ServerTarget>,
    directory: Option<String>,
    load: Load<Loaded>,
    view: View,
    flow: Flow,
    /// Names the sign-in the flow belongs to; an answer for another one is
    /// dropped.
    flow_serial: u64,
    /// The server and directory the flow's attempt lives in.
    flow_owner: Option<(ServerTarget, Option<String>)>,
    poll: Option<Task<()>>,
    /// Integrations with a key write accepted and not answered.
    connecting_keys: HashSet<String>,
    /// Credentials with a removal accepted and not answered.
    removing: HashSet<String>,
    saving_custom: bool,
    custom_protocol: CustomProtocol,
    custom_errors: CustomErrors,
    /// The ID a save of the open custom form already wrote to the config.
    custom_persisted: Option<String>,
    /// Counts accepted hidden-model writes; only the newest one's answer
    /// settles what the rows show.
    hidden_revision: u64,
    failed: Vec<FailedWrite>,
    /// Whether Settings shows this page. A hidden page reads nothing.
    shown: bool,
    /// Retry was pressed; the next render, which has the window, sends it.
    retry_requested: bool,
    /// Feedback of the last action, kept for tests.
    notice: Option<SharedString>,
    /// The sign-in pages handed to the browser, kept for tests.
    #[cfg(test)]
    opened: Vec<String>,
    read_revision: u64,
    /// Writes started; a read that began before one is dropped.
    mutations: u64,
    read: Option<Task<()>>,
    writes: SettingsWrites,
    inputs: Option<Inputs>,
    scroll: ScrollHandle,
    anchors: [gpui_kit::ScrollAnchor; 2],
    focus: HashMap<SharedString, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

/// The directory the providers are read for, as the composer resolves it.
fn directory(owner: &WeakEntity<Workspace>, cx: &App) -> Option<String> {
    owner.upgrade()?.read(cx).config_directory()
}

impl ProvidersPage {
    pub(crate) fn new(owner: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(workspace) = owner.upgrade() {
            subscriptions.push(cx.observe(&workspace, |this, _, cx| {
                let target = ServerTarget::capture(&this.owner, this.network.as_ref(), cx);
                let moved = !same_target(this.target.as_ref(), target.as_ref())
                    || this.directory != directory(&this.owner, cx);
                if !moved {
                    return;
                }
                if this.shown {
                    this.refresh(cx);
                } else if this.target.is_some() {
                    // Hidden: the old server's state is let go without a
                    // read. Showing the page reads the current one.
                    this.retire(cx);
                    this.target = None;
                    this.read = None;
                    cx.notify();
                }
            }));
        }
        let scroll = ScrollHandle::new();
        Self {
            owner,
            network: Network::new().ok(),
            target: None,
            directory: None,
            load: Load::Loading,
            view: View::List,
            flow: Flow::Idle,
            flow_serial: 0,
            flow_owner: None,
            poll: None,
            connecting_keys: HashSet::new(),
            removing: HashSet::new(),
            saving_custom: false,
            custom_protocol: CustomProtocol::default(),
            custom_errors: CustomErrors::default(),
            custom_persisted: None,
            hidden_revision: 0,
            failed: Vec::new(),
            shown: false,
            retry_requested: false,
            notice: None,
            #[cfg(test)]
            opened: Vec::new(),
            read_revision: 0,
            mutations: 0,
            read: None,
            writes: SettingsWrites::default(),
            inputs: None,
            anchors: std::array::from_fn(|_| gpui_kit::ScrollAnchor::for_handle(scroll.clone())),
            scroll,
            focus: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Settings shows or leaves this page. Showing it reads again. Leaving
    /// it drops a sign-in nobody follows and what the secret fields held.
    pub(crate) fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.shown = shown;
        if shown {
            self.refresh(cx);
        } else {
            self.abandon_flow(cx);
            self.inputs = None;
            cx.notify();
        }
    }

    /// OpenCode rebuilt a provider, model or credential catalog: a shown
    /// page reads again. A location's first answer can be partial, and an
    /// account can change outside this page.
    pub(crate) fn catalog_changed(&mut self, cx: &mut Context<Self>) {
        if self.shown {
            self.refresh(cx);
        }
    }

    /// Lets go of everything that belongs to the server and directory shown
    /// so far. Writes already accepted for it still run to their answer.
    fn retire(&mut self, cx: &mut Context<Self>) {
        self.abandon_flow(cx);
        self.load = Load::Loading;
        self.view = View::List;
        self.connecting_keys.clear();
        self.removing.clear();
        self.saving_custom = false;
        self.custom_errors = CustomErrors::default();
        self.custom_persisted = None;
        self.failed.clear();
        // The fields go with their text, the secret ones included.
        self.inputs = None;
    }

    /// Reads the providers, the integrations, the enterprise policy and the
    /// hidden models for the current server and directory. Any failed read
    /// fails the page visibly: a missing list is never shown as "no
    /// providers", and an unread policy never shows the page unlocked.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let target = ServerTarget::capture(&self.owner, self.network.as_ref(), cx);
        let directory = directory(&self.owner, cx);
        if !same_target(self.target.as_ref(), target.as_ref()) || self.directory != directory {
            // Another server's or project's providers never carry over.
            self.retire(cx);
        }
        self.target = target.clone();
        self.directory = directory.clone();
        let Some(target) = target else {
            self.read = None;
            cx.notify();
            return;
        };
        if self.load == Load::Failed {
            self.load = Load::Loading;
        }
        self.read_revision += 1;
        let (revision, mutations) = (self.read_revision, self.mutations);
        let client = target.client;
        let reading = NetworkTask::spawn_on(&target.network, async move {
            let (providers, integrations, policy, settings) = tokio::join!(
                client.listed_providers(directory.as_deref()),
                client.integrations(directory.as_deref()),
                client.enterprise_policy(),
                client.settings()
            );
            let hidden = settings.ok()?.hidden_models;
            Some(Loaded {
                providers: providers.ok()?,
                integrations: integrations.ok()?,
                policy: policy.ok()?,
                confirmed_hidden: hidden.clone(),
                hidden,
            })
        });
        self.read = Some(cx.spawn(async move |this, cx| {
            let result = reading.await.flatten();
            let _ = this.update(cx, |this, cx| {
                if this.read_revision != revision || this.mutations != mutations {
                    return;
                }
                this.read = None;
                match result {
                    Some(loaded) => {
                        // A removal that failed has nothing left to retry
                        // once its account is gone.
                        this.failed.retain(|failed| match failed {
                            FailedWrite::Remove { id, .. } => {
                                loaded.integrations.iter().any(|integration| {
                                    integration.credentials().any(|connection| {
                                        matches!(
                                            connection,
                                            Connection::Credential { id: held, .. } if held == id
                                        )
                                    })
                                })
                            }
                            FailedWrite::Hidden(_) => true,
                        });
                        this.ensure_logos(&loaded, cx);
                        this.load = Load::Ready(loaded);
                    }
                    // What is shown stays through a failed reread.
                    None if this.load.ready().is_some() => {}
                    None => this.load = Load::Failed,
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Asks the workspace for the logos of what the page lists. Enterprise
    /// mode keeps that owner to the bundled ones.
    fn ensure_logos(&self, loaded: &Loaded, cx: &mut Context<Self>) {
        let ids: Vec<String> = loaded
            .providers
            .iter()
            .map(|provider| provider.id.clone())
            .chain(
                loaded
                    .integrations
                    .iter()
                    .map(|integration| integration.id.clone()),
            )
            .collect();
        let owner = self.owner.clone();
        cx.defer(move |cx| {
            let _ = owner.update(cx, |workspace, cx| {
                for id in &ids {
                    workspace.ensure_provider_logo(id, cx);
                }
            });
        });
    }

    /// Whether enterprise mode keeps providers to the OpenCode config: this
    /// machine's policy says so, or the connected server does. A page that
    /// has not read the server's policy is locked.
    fn locked(&self, cx: &App) -> bool {
        let machine = self
            .owner
            .upgrade()
            .is_none_or(|owner| owner.read(cx).enterprise.enterprise_mode());
        machine
            || self.load.ready().is_none_or(|loaded| {
                loaded
                    .policy
                    .as_ref()
                    .is_some_and(|policy| policy.enterprise_mode)
            })
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

    /// Refuses a way in while enterprise mode is on, before anything is
    /// sent. Returns whether it refused.
    fn refuse_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.locked(cx) {
            return false;
        }
        let text = locale::strings(cx).get("settings.providers.enterpriseMode");
        self.toast(text, false, window, cx);
        true
    }

    /// The server refused a way in as enterprise mode: says so, leaves the
    /// connect views and reads the policy again.
    fn refused_by_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = locale::strings(cx).get("settings.providers.enterpriseMode");
        self.toast(text, false, window, cx);
        if matches!(self.view, View::Connect | View::Candidate(_) | View::Custom) {
            self.view = View::List;
        }
        self.clear_secrets(window, cx);
        self.refresh(cx);
    }

    /// Whether a completion still belongs to the connection and directory
    /// that started it.
    fn current(&self, target: &ServerTarget, directory: &Option<String>, cx: &App) -> bool {
        same_target(self.target.as_ref(), Some(target))
            && same_target(
                ServerTarget::capture(&self.owner, self.network.as_ref(), cx).as_ref(),
                Some(target),
            )
            && &self.directory == directory
    }

    /// A provider or credential changed: the composer's model catalog and
    /// the usage samples were read before it, so their owner reads again,
    /// and so does this page.
    fn changed(&mut self, cx: &mut Context<Self>) {
        let owner = self.owner.clone();
        cx.defer(move |cx| {
            let _ = owner.update(cx, |workspace, cx| {
                workspace.refresh_send_catalog(CatalogKind::Credential, None, cx);
            });
        });
        self.refresh(cx);
    }

    fn empty(input: &Entity<InputState>, window: &mut Window, cx: &mut Context<Self>) {
        input.update(cx, |input, cx| input.set_value("", window, cx));
    }

    /// Empties every field that can hold a key or a code.
    fn clear_secrets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(inputs) = &self.inputs else {
            return;
        };
        let secrets = [
            inputs.key.clone(),
            inputs.code.clone(),
            inputs.custom_key.clone(),
        ];
        for secret in &secrets {
            Self::empty(secret, window, cx);
        }
    }

    /// Opens another part of the page. What the secret fields and the
    /// filter held belongs to the part left behind, and so does a sign-in.
    fn open(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(view, View::Connect | View::Candidate(_) | View::Custom)
            && self.refuse_entry(window, cx)
        {
            return;
        }
        if self.view == view {
            return;
        }
        self.abandon_flow(cx);
        self.clear_secrets(window, cx);
        if let Some(inputs) = &self.inputs {
            let search = inputs.search.clone();
            Self::empty(&search, window, cx);
        }
        self.custom_errors = CustomErrors::default();
        self.view = view;
        self.scroll.set_offset(gpui_kit::point(px(0.0), px(0.0)));
        cx.notify();
    }

    /// The integration a key typed in the open view is stored under.
    fn key_integration(&self) -> Option<&str> {
        match &self.view {
            View::Provider(id) | View::Candidate(id) => Some(id),
            _ => None,
        }
    }

    /// Stores the typed key (`ProviderApiKeyForm`). The key goes into the
    /// request and nowhere else; a failure keeps it in the field and says
    /// nothing the server answered.
    fn connect_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() {
            return;
        }
        let Some(integration) = self.key_integration().map(str::to_owned) else {
            return;
        };
        if self.refuse_entry(window, cx) || self.connecting_keys.contains(&integration) {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        let accepted = self
            .load
            .ready()
            .and_then(|loaded| find(&loaded.integrations, &integration))
            // A provider OpenCode has no integration for takes a bare key.
            .is_none_or(|integration| integration.accepts_key && !integration.key_needs_answers);
        if !accepted {
            return;
        }
        let key = self
            .inputs
            .as_ref()
            .and_then(|inputs| SecretKey::new(&inputs.key.read(cx).value()));
        let Some(key) = key else {
            let text = locale::strings(cx).get("settings.providers.page.toast.apiKeyRequired");
            self.toast(text, false, window, cx);
            return;
        };
        self.connecting_keys.insert(integration.clone());
        self.mutations += 1;
        let directory = self.directory.clone();
        let client = target.client.clone();
        let (write_directory, write_integration) = (directory.clone(), integration.clone());
        let (id, writing) = self.writes.submit(&target, async move {
            client
                .connect_provider_key(&write_integration, &key, write_directory.as_deref())
                .await
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<(), ProviderError>> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if !this.current(&target, &directory, cx) {
                    return;
                }
                this.connecting_keys.remove(&integration);
                let strings = locale::strings(cx).clone();
                match result {
                    Some(Ok(())) => {
                        // The field is emptied only while it still belongs
                        // to this provider; another view already did.
                        if this.key_integration() == Some(integration.as_str())
                            && let Some(inputs) = &this.inputs
                        {
                            let key = inputs.key.clone();
                            Self::empty(&key, window, cx);
                        }
                        this.toast(
                            strings.get("settings.providers.page.toast.apiKeySaved"),
                            true,
                            window,
                            cx,
                        );
                        this.connected(&integration, cx);
                    }
                    Some(Err(ProviderError::EnterpriseMode)) => {
                        this.refused_by_server(window, cx);
                    }
                    // Never what the server said: a refusal can quote the
                    // request it refused.
                    _ => this.toast(
                        strings.get("settings.providers.page.toast.apiKeySaveFailed"),
                        false,
                        window,
                        cx,
                    ),
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    /// A way in succeeded (`markAuthWriteSucceeded`): the provider's own
    /// page opens and everything that depends on credentials reads again.
    fn connected(&mut self, provider: &str, cx: &mut Context<Self>) {
        self.reset_flow();
        self.view = View::Provider(provider.to_owned());
        self.mutations += 1;
        self.changed(cx);
    }

    fn reset_flow(&mut self) {
        self.flow_serial += 1;
        self.flow = Flow::Idle;
        self.flow_owner = None;
        self.poll = None;
    }

    /// Drops the sign-in in progress, and its attempt upstream so a
    /// half-finished one cannot be resumed by accident.
    fn abandon_flow(&mut self, cx: &mut Context<Self>) {
        let attempt = self
            .flow
            .attempt()
            .map(|(provider, attempt)| (provider.to_owned(), attempt.id.clone()));
        let owner = self.flow_owner.take();
        self.reset_flow();
        let (Some((provider, attempt)), Some((target, directory))) = (attempt, owner) else {
            return;
        };
        let client = target.client.clone();
        let (id, cancelling) = self.writes.submit(&target, async move {
            let integration = sign_in_integration_id(&provider);
            // Best effort: an attempt left behind expires on its own.
            let _ = client
                .cancel_provider_oauth(integration, &attempt, directory.as_deref())
                .await;
        });
        let completion = cx.spawn(async move |this, cx| {
            let _ = cancelling.await;
            let _ = this.update(cx, |this, cx| {
                this.writes.finish(id);
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
    }

    /// Cancel on the flow's card.
    fn cancel_flow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.abandon_flow(cx);
        if let Some(inputs) = &self.inputs {
            let code = inputs.code.clone();
            Self::empty(&code, window, cx);
        }
        cx.notify();
    }

    fn sign_in_method(&self, provider: &str, method: &str) -> Option<SignInMethod> {
        let loaded = self.load.ready()?;
        find(&loaded.integrations, sign_in_integration_id(provider))?
            .sign_in
            .iter()
            .find(|candidate| candidate.id == method)
            .cloned()
    }

    fn fail_flow(&mut self, provider: String, method: String, message: SharedString) {
        self.reset_flow();
        self.flow = Flow::Failed {
            provider,
            method,
            message,
        };
    }

    /// Starts a sign-in method of the open provider (`beginConnect`). OAuth
    /// opens an attempt and then follows it; an external method stores a
    /// reference to credentials on the server machine.
    fn start_sign_in(&mut self, method: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() || self.flow.busy() {
            return;
        }
        let Some(provider) = self.key_integration().map(str::to_owned) else {
            return;
        };
        if self.refuse_entry(window, cx) {
            return;
        }
        let (Some(target), Some(sign_in)) =
            (self.target.clone(), self.sign_in_method(&provider, &method))
        else {
            return;
        };
        // A required form field needs a client that renders the form.
        if sign_in.needs_answers {
            return;
        }
        self.abandon_flow(cx);
        self.flow_serial += 1;
        let serial = self.flow_serial;
        self.flow = Flow::Connecting {
            provider: provider.clone(),
            method: method.clone(),
        };
        let directory = self.directory.clone();
        self.flow_owner = Some((target.clone(), directory.clone()));
        let client = target.client.clone();
        let integration = sign_in_integration_id(&provider).to_owned();
        let (write_directory, write_method) = (directory.clone(), method.clone());
        let kind = sign_in.kind;
        let (id, writing) = self.writes.submit(&target, async move {
            match kind {
                SignInKind::External => client
                    .connect_provider_external(
                        &integration,
                        &write_method,
                        write_directory.as_deref(),
                    )
                    .await
                    .map(|()| None),
                SignInKind::OAuth => client
                    .start_provider_oauth(&integration, &write_method, write_directory.as_deref())
                    .await
                    .map(Some),
            }
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<Option<OAuthAttempt>, ProviderError>> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                let strings = locale::strings(cx).clone();
                let followed = this.flow_serial == serial && this.current(&target, &directory, cx);
                match result {
                    Some(Ok(Some(attempt))) if followed => {
                        this.follow_attempt(provider, method, attempt, serial, window, cx);
                    }
                    // Nobody follows this attempt any more: it is dropped
                    // upstream and no browser page opens for it.
                    Some(Ok(Some(attempt))) => {
                        this.flow_owner = Some((target, directory));
                        this.flow = Flow::Waiting {
                            provider,
                            method,
                            attempt,
                        };
                        this.abandon_flow(cx);
                    }
                    _ if !followed => {}
                    Some(Ok(None)) => {
                        this.mutations += 1;
                        this.toast(
                            strings.get("settings.providers.page.toast.externalConnected"),
                            true,
                            window,
                            cx,
                        );
                        this.connected(&provider, cx);
                    }
                    Some(Err(ProviderError::EnterpriseMode)) => {
                        this.reset_flow();
                        this.refused_by_server(window, cx);
                    }
                    _ => {
                        let key = match kind {
                            SignInKind::External => {
                                "settings.providers.page.toast.externalConnectFailed"
                            }
                            SignInKind::OAuth => "settings.providers.page.toast.oauthStartFailed",
                        };
                        this.fail_flow(provider, method, strings.get(key));
                    }
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    /// Hands a sign-in page to the browser. Tests record it instead, so no
    /// test opens a page on the developer's desktop.
    fn open_in_browser(&mut self, url: &str, cx: &mut Context<Self>) {
        if !opens_in_browser(url) {
            return;
        }
        #[cfg(test)]
        {
            let _ = cx;
            self.opened.push(url.to_owned());
        }
        #[cfg(not(test))]
        cx.open_url(url);
    }

    /// An attempt opened: its page goes to the browser, then either the code
    /// is asked for or the attempt is polled.
    fn follow_attempt(
        &mut self,
        provider: String,
        method: String,
        attempt: OAuthAttempt,
        serial: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Claude Code's CLI opens its own sign-in page; the attempt's URL is
        // informational (`shouldOpenAuthorizationUrl`).
        if provider != "claude-code" {
            self.open_in_browser(&attempt.url, cx);
        }
        if attempt.mode == OAuthMode::Code {
            self.flow = Flow::AwaitingCode {
                provider,
                method,
                attempt,
                submitting: false,
            };
            return;
        }
        let Some((target, directory)) = self.flow_owner.clone() else {
            return;
        };
        let integration = sign_in_integration_id(&provider).to_owned();
        let attempt_id = attempt.id.clone();
        self.flow = Flow::Waiting {
            provider,
            method,
            attempt,
        };
        self.poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let client = target.client.clone();
                let (integration, attempt_id, directory) =
                    (integration.clone(), attempt_id.clone(), directory.clone());
                let status = NetworkTask::spawn_on(&target.network, async move {
                    client
                        .provider_oauth_status(&integration, &attempt_id, directory.as_deref())
                        .await
                })
                .await;
                let pending = this.update_in(cx, |this, window, cx| {
                    this.settle_poll(serial, status, window, cx)
                });
                if !matches!(pending, Ok(true)) {
                    return;
                }
            }
        }));
    }

    /// Takes one answer of the poll and says whether to ask again. A failed
    /// read ends the sign-in visibly; it is never a pending attempt.
    fn settle_poll(
        &mut self,
        serial: u64,
        status: Option<Result<OAuthStatus, ProviderError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Flow::Waiting {
            provider, method, ..
        } = &self.flow
        else {
            return false;
        };
        if self.flow_serial != serial {
            return false;
        }
        let (provider, method) = (provider.clone(), method.clone());
        let strings = locale::strings(cx).clone();
        let message = match status {
            Some(Ok(OAuthStatus::Pending)) => return true,
            Some(Ok(OAuthStatus::Complete)) => {
                self.toast(
                    strings.get("settings.providers.page.toast.oauthCompleted"),
                    true,
                    window,
                    cx,
                );
                // The poll task is running this; it is let go, not dropped.
                if let Some(poll) = self.poll.take() {
                    poll.detach();
                }
                self.connected(&provider, cx);
                cx.notify();
                return false;
            }
            Some(Ok(OAuthStatus::Failed { message })) if !message.trim().is_empty() => {
                message.into()
            }
            Some(Ok(OAuthStatus::Failed { .. })) => {
                strings.get("settings.providers.page.auth.oauth.error.declined")
            }
            Some(Ok(OAuthStatus::Expired)) => {
                strings.get("settings.providers.page.auth.oauth.error.sessionExpired")
            }
            Some(Err(ProviderError::EnterpriseMode)) => {
                strings.get("settings.providers.enterpriseMode")
            }
            _ => strings.get("settings.providers.page.toast.oauthCompleteFailed"),
        };
        // The attempt is over or unreadable: nothing is left to cancel.
        if let Some(poll) = self.poll.take() {
            poll.detach();
        }
        self.fail_flow(provider, method, message);
        cx.notify();
        false
    }

    /// Hands the pasted code to the attempt (`submitCode`).
    fn submit_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() || self.refuse_entry(window, cx) {
            return;
        }
        let Flow::AwaitingCode {
            provider,
            method,
            attempt,
            submitting: false,
        } = self.flow.clone()
        else {
            return;
        };
        let Some((target, directory)) = self.flow_owner.clone() else {
            return;
        };
        let code = self
            .inputs
            .as_ref()
            .and_then(|inputs| SecretKey::new(&inputs.code.read(cx).value()));
        let Some(code) = code else {
            return;
        };
        let serial = self.flow_serial;
        self.flow = Flow::AwaitingCode {
            provider: provider.clone(),
            method: method.clone(),
            attempt: attempt.clone(),
            submitting: true,
        };
        let client = target.client.clone();
        let integration = sign_in_integration_id(&provider).to_owned();
        let write_directory = directory.clone();
        let (id, writing) = self.writes.submit(&target, async move {
            client
                .complete_provider_oauth(
                    &integration,
                    &attempt.id,
                    &code,
                    write_directory.as_deref(),
                )
                .await
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<(), ProviderError>> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if this.flow_serial != serial || !this.current(&target, &directory, cx) {
                    return;
                }
                if let Some(inputs) = &this.inputs {
                    let code = inputs.code.clone();
                    Self::empty(&code, window, cx);
                }
                let strings = locale::strings(cx).clone();
                match result {
                    Some(Ok(())) => {
                        this.toast(
                            strings.get("settings.providers.page.toast.oauthCompleted"),
                            true,
                            window,
                            cx,
                        );
                        this.connected(&provider, cx);
                    }
                    Some(Err(ProviderError::EnterpriseMode)) => {
                        this.reset_flow();
                        this.refused_by_server(window, cx);
                    }
                    _ => this.fail_flow(
                        provider,
                        method,
                        strings.get("settings.providers.page.toast.oauthCompleteFailed"),
                    ),
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    /// Removes one stored account (`handleRemoveAccount`). It only narrows
    /// access, so enterprise mode allows it. The row stays until the server
    /// confirms, and an account with a removal in flight takes no second
    /// one.
    fn remove_credential(
        &mut self,
        id: String,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.writes.closing() || self.removing.contains(&id) {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        self.removing.insert(id.clone());
        self.failed.retain(
            |failed| !matches!(failed, FailedWrite::Remove { id: earlier, .. } if earlier == &id),
        );
        self.mutations += 1;
        let directory = self.directory.clone();
        let client = target.client.clone();
        let removed = id.clone();
        let (write, writing) = self.writes.submit(&target, async move {
            client.remove_provider_credential(&removed).await
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<(), ProviderError>> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(write);
                if !this.current(&target, &directory, cx) {
                    return;
                }
                this.removing.remove(&id);
                let strings = locale::strings(cx).clone();
                if matches!(result, Some(Ok(()))) {
                    this.toast(
                        strings.get("settings.providers.accounts.toast.removed"),
                        true,
                        window,
                        cx,
                    );
                } else {
                    this.failed.push(FailedWrite::Remove {
                        id: id.clone(),
                        label: label.clone(),
                    });
                    this.toast(
                        strings.get("settings.providers.accounts.toast.removeFailed"),
                        false,
                        window,
                        cx,
                    );
                }
                // Either way everything reads again: after a failure the
                // account may or may not still exist.
                this.mutations += 1;
                this.changed(cx);
                cx.notify();
            });
        });
        self.writes.observe(write, completion);
        cx.notify();
    }

    /// Writes the hidden models. The rows show the new list at once. The
    /// newest write's answer becomes the confirmed list; its failure puts
    /// the rows back on the confirmed one and keeps the change for Retry.
    fn set_hidden(
        &mut self,
        hidden: Vec<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.writes.closing() {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        let Some(loaded) = self.load.ready_mut() else {
            return;
        };
        if loaded.hidden == hidden {
            return;
        }
        loaded.hidden.clone_from(&hidden);
        self.failed
            .retain(|failed| !matches!(failed, FailedWrite::Hidden(_)));
        self.hidden_revision += 1;
        let revision = self.hidden_revision;
        self.mutations += 1;
        let directory = self.directory.clone();
        let client = target.client.clone();
        let sent = hidden.clone();
        let (id, writing) =
            self.writes.submit(
                &target,
                async move { client.save_hidden_models(&sent).await },
            );
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<Vec<(String, String)>, ProviderError>> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if !this.current(&target, &directory, cx) {
                    return;
                }
                let newest = this.hidden_revision == revision;
                match result {
                    Some(Ok(answer)) => {
                        if let Some(loaded) = this.load.ready_mut() {
                            loaded.confirmed_hidden.clone_from(&answer);
                            if newest {
                                loaded.hidden.clone_from(&answer);
                            }
                        }
                        // The pickers hide by the settings the workspace
                        // holds.
                        let owner = this.owner.clone();
                        cx.defer(move |cx| {
                            let _ = owner.update(cx, |workspace, cx| {
                                if let Some(settings) = &mut workspace.send_catalog.settings {
                                    settings.hidden_models = answer;
                                }
                                cx.notify();
                            });
                        });
                    }
                    _ if newest => {
                        if let Some(loaded) = this.load.ready_mut() {
                            loaded.hidden = loaded.confirmed_hidden.clone();
                        }
                        this.failed.push(FailedWrite::Hidden(hidden));
                        let text = locale::strings(cx).get("settings.common.status.saveFailed");
                        this.toast(text, false, window, cx);
                    }
                    // An older write failed and a newer one is in flight
                    // with the whole list.
                    _ => {}
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    fn toggle_hidden(
        &mut self,
        provider: &str,
        model: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(loaded) = self.load.ready() else {
            return;
        };
        let mut hidden = loaded.hidden.clone();
        let entry = (provider.to_owned(), model.to_owned());
        match hidden.iter().position(|held| held == &entry) {
            Some(index) => {
                hidden.remove(index);
            }
            None => hidden.push(entry),
        }
        self.set_hidden(hidden, window, cx);
    }

    /// Hide all or Show all for one provider.
    fn set_all_hidden(
        &mut self,
        provider: &str,
        hide: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(loaded) = self.load.ready() else {
            return;
        };
        let mut hidden: Vec<(String, String)> = loaded
            .hidden
            .iter()
            .filter(|(held, _)| held != provider)
            .cloned()
            .collect();
        if hide && let Some(listed) = loaded.providers.iter().find(|listed| listed.id == provider) {
            hidden.extend(
                listed
                    .models
                    .iter()
                    .map(|model| (provider.to_owned(), model.id.clone())),
            );
        }
        self.set_hidden(hidden, window, cx);
    }

    fn custom_draft(&self, cx: &App) -> Option<CustomDraft> {
        let inputs = self.inputs.as_ref()?;
        let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
        Some(CustomDraft {
            id: text(&inputs.custom_id),
            name: text(&inputs.custom_name),
            base_url: text(&inputs.custom_url),
            protocol: self.custom_protocol,
            models: inputs
                .models
                .iter()
                .map(|row| (text(&row.id), text(&row.name)))
                .collect(),
        })
    }

    /// Saves the custom provider (`handleSaveCustomProvider`): the config
    /// first, then the key, because OpenCode accepts a key for it only once
    /// the provider is in its config.
    fn save_custom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() || self.saving_custom || self.refuse_entry(window, cx) {
            return;
        }
        let (Some(target), Some(draft), Some(loaded)) = (
            self.target.clone(),
            self.custom_draft(cx),
            self.load.ready(),
        ) else {
            return;
        };
        let (credential, key) = match &self.inputs {
            Some(inputs) => draft_credential(&inputs.custom_key.read(cx).value()),
            None => return,
        };
        let existing: HashSet<&str> = loaded
            .providers
            .iter()
            .map(|provider| provider.id.as_str())
            .collect();
        let provider = match validate_custom(
            &draft,
            &credential,
            &existing,
            self.custom_persisted.as_deref(),
        ) {
            Ok(provider) => provider,
            Err(errors) => {
                self.custom_errors = errors;
                cx.notify();
                return;
            }
        };
        self.custom_errors = CustomErrors::default();
        self.saving_custom = true;
        self.mutations += 1;
        let directory = self.directory.clone();
        let client = target.client.clone();
        let write_directory = directory.clone();
        let written = provider.clone();
        let (id, writing) = self.writes.submit(&target, async move {
            let directory = write_directory.as_deref();
            if let Err(error) = client.save_custom_provider(&written, directory).await {
                return CustomOutcome::ConfigFailed(error);
            }
            let Some(key) = key else {
                return CustomOutcome::Saved;
            };
            let mut waited = 0;
            loop {
                match client
                    .connect_provider_key(&written.id, &key, directory)
                    .await
                {
                    Ok(()) => return CustomOutcome::Saved,
                    Err(error) if error.is_not_found() && waited < KEY_RETRY_DELAYS.len() => {
                        tokio::time::sleep(KEY_RETRY_DELAYS[waited]).await;
                        waited += 1;
                    }
                    Err(error) => return CustomOutcome::KeyFailed(error),
                }
            }
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<CustomOutcome> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if !this.current(&target, &directory, cx) {
                    return;
                }
                this.saving_custom = false;
                let strings = locale::strings(cx).clone();
                let failed = strings.get("settings.providers.page.toast.customProviderSaveFailed");
                match result {
                    Some(CustomOutcome::Saved) => {
                        this.custom_persisted = None;
                        this.reset_custom(window, cx);
                        this.toast(
                            strings.format(
                                "settings.providers.page.toast.customProviderSaved",
                                &[("provider", &provider.name)],
                            ),
                            true,
                            window,
                            cx,
                        );
                        this.connected(&provider.id, cx);
                    }
                    Some(CustomOutcome::ConfigFailed(ProviderError::EnterpriseMode)) => {
                        this.refused_by_server(window, cx);
                    }
                    // The config write carries no key, so the server's own
                    // words are safe to show.
                    Some(CustomOutcome::ConfigFailed(ProviderError::Refused {
                        message: Some(message),
                        ..
                    })) if !message.trim().is_empty() => {
                        this.toast(message.into(), false, window, cx);
                    }
                    Some(CustomOutcome::KeyFailed(_)) => {
                        // The provider exists now; saving again sends the
                        // key again.
                        this.custom_persisted = Some(provider.id.clone());
                        this.toast(failed, false, window, cx);
                        this.mutations += 1;
                        this.changed(cx);
                    }
                    _ => this.toast(failed, false, window, cx),
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    /// Empties the custom provider form down to one model row.
    fn reset_custom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.custom_protocol = CustomProtocol::default();
        self.custom_errors = CustomErrors::default();
        let Some(inputs) = &mut self.inputs else {
            return;
        };
        inputs.models.truncate(1);
        let fields: Vec<Entity<InputState>> = [
            &inputs.custom_id,
            &inputs.custom_name,
            &inputs.custom_url,
            &inputs.custom_key,
        ]
        .into_iter()
        .chain(inputs.models.iter().flat_map(|row| [&row.id, &row.name]))
        .cloned()
        .collect();
        for field in &fields {
            Self::empty(field, window, cx);
        }
    }

    fn model_row(window: &mut Window, cx: &mut Context<Self>) -> ModelRow {
        let strings = locale::strings(cx).clone();
        let mut field = |placeholder: &str| {
            let placeholder = strings.get(placeholder);
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        ModelRow {
            id: field("settings.providers.page.custom.models.idPlaceholder"),
            name: field("settings.providers.page.custom.models.namePlaceholder"),
        }
    }

    fn add_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = Self::model_row(window, cx);
        if let Some(inputs) = &mut self.inputs {
            inputs.models.push(row);
            cx.notify();
        }
    }

    fn remove_model(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(inputs) = &mut self.inputs
            && inputs.models.len() > 1
            && index < inputs.models.len()
        {
            inputs.models.remove(index);
            self.custom_errors = CustomErrors::default();
            cx.notify();
        }
    }

    /// Creates the text fields on the first render that needs them. The
    /// secret ones are masked and expose no value to assistive technology.
    fn ensure_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inputs.is_some() {
            return;
        }
        let strings = locale::strings(cx).clone();
        let mut field = |placeholder: &str, secret: bool| {
            let placeholder = strings.get(placeholder);
            cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(secret)
                    .placeholder(placeholder)
            })
        };
        let search = field("settings.providers.grid.searchPlaceholder", false);
        let key = field("settings.providers.page.auth.apiKeyPlaceholder", true);
        let code = field(
            "settings.providers.page.auth.pasteAuthorizationCodePlaceholder",
            true,
        );
        let custom_id = field(
            "settings.providers.page.custom.field.providerID.placeholder",
            false,
        );
        let custom_name = field(
            "settings.providers.page.custom.field.name.placeholder",
            false,
        );
        let custom_url = field(
            "settings.providers.page.custom.field.baseURL.placeholder",
            false,
        );
        let custom_key = field(
            "settings.providers.page.custom.field.apiKey.placeholder",
            true,
        );
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |_, _, _: &InputEvent, _, cx| cx.notify()),
            cx.subscribe_in(&key, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.connect_key(window, cx);
                }
            }),
            cx.subscribe_in(&code, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit_code(window, cx);
                }
            }),
        ];
        let models = vec![Self::model_row(window, cx)];
        self.inputs = Some(Inputs {
            search,
            key,
            code,
            custom_id,
            custom_name,
            custom_url,
            custom_key,
            models,
            _subscriptions: subscriptions,
        });
    }

    /// Sends every kept change again.
    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for failed in std::mem::take(&mut self.failed) {
            match failed {
                FailedWrite::Hidden(hidden) => self.set_hidden(hidden, window, cx),
                FailedWrite::Remove { id, label } => self.remove_credential(id, label, window, cx),
            }
        }
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
        !self.failed.is_empty()
    }

    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.writes.cancel_close();
        cx.notify();
    }

    pub(crate) fn take_quit_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.writes.take_quit_write(self.network.take(), cx)
    }

    /// Opens what a Settings search result names. In enterprise mode the
    /// connect results open the list, which says why.
    pub(crate) fn reveal(&mut self, item: &str, window: &mut Window, cx: &mut Context<Self>) {
        match item {
            "providers.connect" => self.open(View::Connect, window, cx),
            "providers.custom" => self.open(View::Custom, window, cx),
            "providers.auth" | "providers.models" => {
                let index = if item == "providers.auth" {
                    ACCOUNTS
                } else {
                    MODELS
                };
                cx.on_next_frame(window, move |this, window, cx| {
                    this.anchors[index].scroll_to(window, cx);
                });
            }
            _ => {}
        }
    }
}

impl ProvidersPage {
    fn text_button(
        &mut self,
        id: SharedString,
        label: SharedString,
        enabled: bool,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        on_activate(
            gpui_kit::base::Button::new(id.clone())
                .accessibility_id(id)
                .accessibility_label(label.clone())
                .disabled(!enabled || self.writes.closing())
                .h(gpui_kit::rems(1.75))
                .px(px(8.0))
                .rounded(px(7.0))
                .child(label),
            cx,
            action,
        )
        .into_any_element()
    }

    fn logo(&self, provider: &str, size: f32, cx: &App) -> AnyElement {
        let palette = cx.global::<Palette>();
        self.owner
            .upgrade()
            .and_then(|owner| {
                owner
                    .read(cx)
                    .render_provider_logo(provider, size, 1.0, palette)
            })
            .unwrap_or_else(|| glyph("server", size, palette.muted_foreground).into_any_element())
    }

    /// One clickable row of a list: a logo, a name and what follows it.
    fn list_row(
        &mut self,
        id: SharedString,
        provider: &str,
        name: SharedString,
        detail: Option<(SharedString, gpui_kit::Hsla)>,
        bordered: bool,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let palette = cx.global::<Palette>().clone();
        let direction = locale::strings(cx).direction();
        let focus = focus_handle(&mut self.focus, &id, cx);
        let hover = palette.hover;
        let logo = self.logo(provider, 16.0, cx);
        on_activate(
            div()
                .id(id.clone())
                .accessibility_id(id)
                .role(Role::Button)
                .aria_label(name.clone())
                .track_focus(&focus)
                .tab_index(0)
                .flex_row_from(direction)
                .items_center()
                .gap(px(12.0))
                .px(px(16.0))
                .py(px(10.0))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .when(bordered, |row| {
                    row.border_b_1().border_color(palette.border)
                })
                .child(logo)
                .child(div().flex_1().min_w_0().truncate().child(name))
                .children(detail.map(|(text, color)| {
                    div()
                        .flex_shrink_0()
                        .css_text(0.75, 1.0, cx)
                        .text_color(color)
                        .child(text)
                })),
            cx,
            action,
        )
        .test_support()
        .into_any_element()
    }

    fn list_box(id: &'static str, cx: &App) -> gpui_kit::Stateful<gpui_kit::Div> {
        div()
            .id(id)
            .rounded(px(10.0))
            .bg(cx.global::<Palette>().elevated)
            .overflow_hidden()
            .flex()
            .flex_col()
    }

    /// The filter field of the list in view.
    fn search_field(
        &self,
        placeholder: SharedString,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let Some(inputs) = &self.inputs else {
            return div().into_any_element();
        };
        let search = inputs.search.clone();
        if search.read(cx).presentation().placeholder() != &placeholder {
            let shown = placeholder.clone();
            search.update(cx, |input, cx| input.set_placeholder(shown, window, cx));
        }
        div()
            .mb(px(12.0))
            .max_w(px(320.0))
            .child(
                Input::new(&search)
                    .id("providers-search")
                    .accessibility_id("providers-search")
                    .aria_label(placeholder),
            )
            .into_any_element()
    }

    fn query(&self, cx: &App) -> String {
        self.inputs
            .as_ref()
            .map(|inputs| inputs.search.read(cx).value().trim().to_lowercase())
            .unwrap_or_default()
    }

    /// Who manages the app and what that means here
    /// (`EnterpriseProvidersNotice`).
    fn enterprise_notice(&self, loaded: &Loaded, cx: &App) -> AnyElement {
        let strings = locale::strings(cx);
        let palette = cx.global::<Palette>();
        let direction = strings.direction();
        let policy = loaded.policy.as_ref();
        let organization = policy.and_then(|policy| policy.organization.as_deref());
        div()
            .id("providers-enterprise-notice")
            .accessibility_id("providers-enterprise-notice")
            .role(Role::Status)
            .aria_label(strings.get("settings.providers.enterpriseTitle"))
            .mb(px(16.0))
            .p(px(12.0))
            .rounded(px(8.0))
            .bg(palette.status_info.opacity(0.12))
            .flex_row_from(direction)
            .items_start()
            .gap(px(8.0))
            .child(
                div()
                    .pt(px(2.0))
                    .child(glyph("information", 16.0, palette.status_info)),
            )
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(strings.get("settings.providers.enterpriseTitle")),
                    )
                    .children(organization.map(|organization| {
                        div()
                            .id("providers-enterprise-managed-by")
                            .css_text(0.8125, 1.25, cx)
                            .child(strings.format(
                                "settings.providers.enterpriseManagedBy",
                                &[("organization", organization)],
                            ))
                    }))
                    .children(
                        policy
                            .is_some_and(|policy| policy.policy_unreadable)
                            .then(|| {
                                div().css_text(0.8125, 1.25, cx).child(
                                    strings.get("settings.providers.enterprisePolicyUnreadable"),
                                )
                            }),
                    )
                    .child(helper(strings.get("settings.providers.enterpriseMode"), cx)),
            )
            .test_support()
            .into_any_element()
    }

    fn status_label(
        status: CardStatus,
        strings: &Strings,
        palette: &Palette,
    ) -> (SharedString, gpui_kit::Hsla) {
        match status {
            CardStatus::ReauthNeeded => (
                strings.get("settings.providers.card.status.reauthNeeded"),
                palette.status_warning,
            ),
            CardStatus::Accounts(count) => (
                strings.format(
                    "settings.providers.card.status.accounts",
                    &[("count", &count.to_string())],
                ),
                palette.status_success,
            ),
            CardStatus::Connected => (
                strings.get("settings.providers.card.status.connected"),
                palette.status_success,
            ),
            CardStatus::NoModels => (
                strings.get("settings.providers.card.status.noModels"),
                palette.muted_foreground,
            ),
            CardStatus::Environment => (
                strings.get("settings.providers.card.status.environment"),
                palette.muted_foreground,
            ),
            CardStatus::SignInNeeded => (
                strings.get("settings.providers.card.status.signInNeeded"),
                palette.muted_foreground,
            ),
        }
    }

    /// Every listed provider (`ProviderGrid`).
    fn list_view(
        &mut self,
        loaded: &Loaded,
        locked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let query = self.query(cx);
        let shown = shown_providers(&loaded.providers, &loaded.integrations);
        let matching: Vec<&ListedProvider> = shown
            .iter()
            .copied()
            .filter(|provider| {
                query.is_empty()
                    || provider.id.to_lowercase().contains(&query)
                    || provider
                        .name
                        .as_deref()
                        .is_some_and(|name| name.to_lowercase().contains(&query))
            })
            .collect();
        let mut children = Vec::new();
        if locked {
            children.push(self.enterprise_notice(loaded, cx));
        }
        if !shown.is_empty() {
            children.push(self.search_field(
                strings.get("settings.providers.grid.searchPlaceholder"),
                window,
                cx,
            ));
        }
        // The one way in to connecting a provider. Enterprise mode has none.
        if !locked && query.is_empty() {
            children.push(
                div()
                    .mb(px(12.0))
                    .child(self.text_button(
                        "providers-connect".into(),
                        strings.get("settings.providers.grid.connect"),
                        true,
                        cx,
                        |this, window, cx| this.open(View::Connect, window, cx),
                    ))
                    .into_any_element(),
            );
        }
        if shown.is_empty() {
            children.push(
                div()
                    .id("providers-empty")
                    .py(px(24.0))
                    .child(helper(strings.get("settings.providers.grid.empty"), cx))
                    .test_support()
                    .into_any_element(),
            );
            return children;
        }
        if matching.is_empty() {
            children.push(helper(
                strings.format("settings.providers.grid.noMatches", &[("query", &query)]),
                cx,
            ));
            return children;
        }
        let mut list = Self::list_box("providers-list", cx);
        let count = matching.len();
        for (index, provider) in matching.into_iter().enumerate() {
            let status = card_status(&loaded.integrations, provider)
                .map(|status| Self::status_label(status, &strings, &palette));
            let id = provider.id.clone();
            list = list.child(
                self.list_row(
                    format!("providers-row-{}", provider.id).into(),
                    &provider.id,
                    provider
                        .name
                        .clone()
                        .unwrap_or_else(|| provider.id.clone())
                        .into(),
                    status,
                    index + 1 < count,
                    cx,
                    move |this, window, cx| this.open(View::Provider(id.clone()), window, cx),
                ),
            );
        }
        children.push(list.into_any_element());
        children
    }

    /// What can still be connected (`ProviderConnectPicker`): the popular
    /// providers first, then the rest by name, then the custom provider.
    fn connect_view(
        &mut self,
        loaded: &Loaded,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let query = self.query(cx);
        let listed: HashSet<&str> = shown_providers(&loaded.providers, &loaded.integrations)
            .into_iter()
            .map(|provider| provider.id.as_str())
            .collect();
        let candidates: Vec<&Integration> = connectable(&loaded.integrations, &listed)
            .into_iter()
            .filter(|integration| {
                query.is_empty()
                    || integration.id.to_lowercase().contains(&query)
                    || integration.name.to_lowercase().contains(&query)
            })
            .collect();
        let popular: Vec<(&Integration, &str)> = POPULAR
            .iter()
            .filter_map(|(id, note)| {
                candidates
                    .iter()
                    .find(|integration| integration.id == *id)
                    .map(|integration| (*integration, *note))
            })
            .collect();
        let others: Vec<&Integration> = candidates
            .iter()
            .copied()
            .filter(|integration| !POPULAR.iter().any(|(id, _)| *id == integration.id))
            .collect();
        let mut children = vec![
            self.sub_header(
                strings.get("settings.providers.page.connect.title"),
                None,
                View::List,
                cx,
            ),
            self.search_field(
                strings.get("settings.providers.connect.searchPlaceholder"),
                window,
                cx,
            ),
        ];
        let mut groups: Vec<(&'static str, Vec<(&Integration, Option<&str>)>)> = Vec::new();
        if !popular.is_empty() {
            groups.push((
                "settings.providers.connect.popular",
                popular
                    .into_iter()
                    .map(|(integration, note)| (integration, Some(note)))
                    .collect(),
            ));
        }
        if !others.is_empty() {
            groups.push((
                "settings.providers.connect.all",
                others
                    .into_iter()
                    .map(|integration| (integration, None))
                    .collect(),
            ));
        }
        if groups.is_empty() {
            children.push(
                div()
                    .id("providers-connect-empty")
                    .py(px(12.0))
                    .child(helper(
                        strings.get("settings.providers.page.connect.noProvidersFound"),
                        cx,
                    ))
                    .test_support()
                    .into_any_element(),
            );
        }
        for (title, rows) in groups {
            let mut list = Self::list_box(title, cx);
            let count = rows.len();
            for (index, (integration, note)) in rows.into_iter().enumerate() {
                let id = integration.id.clone();
                list = list.child(self.list_row(
                    format!("providers-candidate-{}", integration.id).into(),
                    &integration.id,
                    integration.name.clone().into(),
                    note.map(|note| (strings.get(note), palette.muted_foreground)),
                    index + 1 < count,
                    cx,
                    move |this, window, cx| this.open(View::Candidate(id.clone()), window, cx),
                ));
            }
            children.push(
                div()
                    .mb(px(16.0))
                    .child(
                        div()
                            .mb(px(6.0))
                            .css_text(0.8125, 1.25, cx)
                            .text_color(palette.muted_foreground)
                            .child(strings.get(title)),
                    )
                    .child(list)
                    .into_any_element(),
            );
        }
        if query.is_empty() {
            let row = self.list_row(
                "providers-custom".into(),
                "",
                strings.get("settings.providers.page.custom.title"),
                Some((
                    strings.get("settings.providers.connect.customHint"),
                    palette.muted_foreground,
                )),
                false,
                cx,
                |this, window, cx| this.open(View::Custom, window, cx),
            );
            children.push(
                Self::list_box("providers-custom-box", cx)
                    .child(row)
                    .into_any_element(),
            );
        }
        children
    }

    /// Back, a logo and the name of what the view is about.
    fn sub_header(
        &mut self,
        title: SharedString,
        logo: Option<&str>,
        back: View,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let back_button = self.text_button(
            "providers-back".into(),
            strings.get("settings.providers.page.back"),
            true,
            cx,
            move |this, window, cx| this.open(back.clone(), window, cx),
        );
        div()
            .mb(px(20.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(10.0))
            .child(back_button)
            .children(logo.map(|provider| self.logo(provider, 18.0, cx)))
            .child(
                div()
                    .id("providers-view-title")
                    .role(Role::Heading)
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .into_any_element()
    }

    /// The key field and Save key. The field is a password input: masked on
    /// screen and without a value for assistive technology.
    fn key_form(&mut self, integration: &str, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let Some(inputs) = &self.inputs else {
            return div().into_any_element();
        };
        let key = inputs.key.clone();
        let saving = self.connecting_keys.contains(integration);
        let save = self.text_button(
            "providers-key-save".into(),
            strings.get(if saving {
                "settings.providers.page.actions.saving"
            } else {
                "settings.providers.page.actions.saveKey"
            }),
            !saving,
            cx,
            |this, window, cx| this.connect_key(window, cx),
        );
        div()
            .id("providers-key-form")
            .flex_row_from(direction)
            .flex_wrap()
            .items_center()
            .gap(px(8.0))
            .child(
                div().w(px(320.0)).child(
                    Input::new(&key)
                        .id("providers-key")
                        .accessibility_id("providers-key")
                        .content_type(InputContentType::Password)
                        .aria_label(strings.get("settings.providers.page.auth.apiKeyLabel"))
                        .disabled(saving),
                ),
            )
            .child(save)
            .child(info_hint(
                "providers.key",
                strings.get("settings.providers.page.auth.apiKeyTooltip"),
                cx,
            ))
            .into_any_element()
    }

    /// The sign-in in progress for `provider`: what to do in the browser,
    /// the device code, the code field, or why it failed.
    fn flow_card(&mut self, provider: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.flow.provider() != Some(provider) {
            return None;
        }
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let flow = self.flow.clone();
        let mut card = div()
            .id("providers-flow")
            .accessibility_id("providers-flow")
            .role(Role::Status)
            .p(px(12.0))
            .rounded(px(8.0))
            .bg(palette.elevated)
            .flex()
            .flex_col()
            .gap(px(8.0));
        let cancel = self.text_button(
            "providers-flow-cancel".into(),
            strings.get("settings.providers.page.actions.cancel"),
            true,
            cx,
            |this, window, cx| this.cancel_flow(window, cx),
        );
        match flow {
            Flow::Idle => return None,
            Flow::Connecting { .. } => {
                card = card
                    .aria_label(strings.get("settings.providers.page.auth.oauth.starting"))
                    .child(helper(
                        strings.get("settings.providers.page.auth.oauth.starting"),
                        cx,
                    ));
            }
            Flow::Failed {
                message, method, ..
            } => {
                let again = self.text_button(
                    "providers-flow-retry".into(),
                    strings.get("settings.providers.page.actions.tryAgain"),
                    true,
                    cx,
                    move |this, window, cx| this.start_sign_in(method.clone(), window, cx),
                );
                card = card
                    .aria_label(message.clone())
                    .child(
                        div()
                            .css_text(0.8125, 1.25, cx)
                            .text_color(palette.status_error)
                            .child(message),
                    )
                    .child(
                        div()
                            .flex_row_from(direction)
                            .gap(px(8.0))
                            .child(again)
                            .child(cancel),
                    );
            }
            Flow::Waiting { attempt, .. } | Flow::AwaitingCode { attempt, .. } => {
                let awaiting_code = matches!(self.flow, Flow::AwaitingCode { .. });
                let submitting = matches!(
                    self.flow,
                    Flow::AwaitingCode {
                        submitting: true,
                        ..
                    }
                );
                let title = strings.get(if awaiting_code {
                    "settings.providers.page.auth.oauth.codeHint"
                } else {
                    "settings.providers.page.auth.oauth.waiting"
                });
                card = card.aria_label(title.clone()).child(title);
                if !awaiting_code {
                    card = card.child(helper(
                        strings.get("settings.providers.page.auth.oauth.waitingHint"),
                        cx,
                    ));
                }
                if !attempt.instructions.trim().is_empty() {
                    card = card.child(helper(attempt.instructions.clone().into(), cx));
                }
                if let Some(code) = user_code(&attempt.instructions) {
                    let copied = code.to_owned();
                    let copy = self.text_button(
                        "providers-flow-copy-code".into(),
                        strings.get("settings.providers.page.actions.copyCode"),
                        true,
                        cx,
                        move |this, window, cx| {
                            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                copied.clone(),
                            ));
                            let text = locale::strings(cx)
                                .get("settings.providers.page.toast.deviceCodeCopied");
                            this.toast(text, true, window, cx);
                        },
                    );
                    card =
                        card.child(
                            div()
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .id("providers-flow-device-code")
                                        .aria_label(strings.get(
                                            "settings.providers.page.auth.oauth.deviceCodeLabel",
                                        ))
                                        .font_family(
                                            gpui_kit::component::Theme::global(cx)
                                                .mono_font_family
                                                .clone(),
                                        )
                                        .child(SharedString::from(code.to_owned())),
                                )
                                .child(copy),
                        );
                }
                let mut actions = div().flex_row_from(direction).flex_wrap().gap(px(8.0));
                if opens_in_browser(&attempt.url) {
                    let url = attempt.url.clone();
                    actions = actions.child(self.text_button(
                        "providers-flow-open".into(),
                        strings.get("settings.providers.page.actions.open"),
                        true,
                        cx,
                        move |this, _, cx| this.open_in_browser(&url, cx),
                    ));
                }
                if awaiting_code && let Some(inputs) = &self.inputs {
                    let code = inputs.code.clone();
                    let complete = self.text_button(
                        "providers-code-submit".into(),
                        strings.get("settings.providers.page.actions.complete"),
                        !submitting,
                        cx,
                        |this, window, cx| this.submit_code(window, cx),
                    );
                    card = card.child(
                        div()
                            .flex_row_from(direction)
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div().w(px(320.0)).child(
                                    Input::new(&code)
                                        .id("providers-code")
                                        .accessibility_id("providers-code")
                                        .content_type(InputContentType::Password)
                                        .aria_label(strings.get(
                                            "settings.providers.page.auth.pasteAuthorizationCodePlaceholder",
                                        ))
                                        .disabled(submitting),
                                ),
                            )
                            .child(complete),
                    );
                }
                card = card.child(actions.child(cancel));
            }
        }
        Some(card.test_support().into_any_element())
    }

    /// The ways to add credentials to `provider`: its key field and its
    /// sign-in buttons. Only called while enterprise mode is off.
    fn add_methods(
        &mut self,
        provider: &str,
        loaded: &Loaded,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let own = find(&loaded.integrations, provider);
        let sign_in: Vec<SignInMethod> =
            find(&loaded.integrations, sign_in_integration_id(provider))
                .map(|integration| integration.sign_in.clone())
                .unwrap_or_default();
        let mut rows = Vec::new();
        // An integration OpenCode does not know is still keyed by a key.
        let offers_key = own.is_none_or(|integration| integration.accepts_key);
        let key_supported = own.is_none_or(|integration| !integration.key_needs_answers);
        if offers_key && key_supported {
            rows.push(self.key_form(provider, cx));
        }
        let busy = self.flow.busy();
        let mut buttons = div().flex_row_from(direction).flex_wrap().gap(px(8.0));
        let mut offered = 0;
        for (index, method) in sign_in.iter().enumerate() {
            if method.needs_answers {
                continue;
            }
            offered += 1;
            let label: SharedString = match &method.label {
                Some(label) => label.clone().into(),
                None => strings.format(
                    "settings.providers.page.auth.oauthMethodFallback",
                    &[("index", &(index + 1).to_string())],
                ),
            };
            let method_id = method.id.clone();
            buttons = buttons.child(self.text_button(
                format!("providers-sign-in-{}", method.id).into(),
                label,
                !busy,
                cx,
                move |this, window, cx| this.start_sign_in(method_id.clone(), window, cx),
            ));
        }
        if offered > 0 {
            rows.push(buttons.into_any_element());
        }
        // A method whose form this page cannot answer is left to OpenCode's
        // own connect command.
        if rows.is_empty() {
            rows.push(
                div()
                    .id("providers-no-method")
                    .child(helper(strings.get("common.unavailable"), cx))
                    .test_support()
                    .into_any_element(),
            );
        }
        rows.extend(self.flow_card(provider, cx));
        rows
    }

    /// A provider chosen on the connect list.
    fn candidate_view(
        &mut self,
        id: &str,
        loaded: &Loaded,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let name: SharedString = find(&loaded.integrations, id)
            .map_or_else(|| id.to_owned(), |integration| integration.name.clone())
            .into();
        let header = self.sub_header(name, Some(id), View::Connect, cx);
        let methods = self.add_methods(id, loaded, cx);
        vec![
            header,
            section_with_info(
                "settings.providers.page.auth.title",
                None,
                true,
                methods,
                Some(self.anchors[ACCOUNTS].clone()),
                cx,
            ),
        ]
    }

    fn account_row(&mut self, connection: &Connection, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let (row_id, label, detail, remove): (SharedString, SharedString, SharedString, _) =
            match connection {
                Connection::Credential {
                    id, label, method, ..
                } => {
                    let (removed, removed_label) = (id.clone(), label.clone());
                    let pending = self.removing.contains(id);
                    let remove = self.text_button(
                        format!("providers-account-remove-{id}").into(),
                        strings.get("settings.providers.accounts.remove"),
                        !pending,
                        cx,
                        move |this, window, cx| {
                            this.remove_credential(
                                removed.clone(),
                                removed_label.clone(),
                                window,
                                cx,
                            );
                        },
                    );
                    (
                        format!("providers-account-{id}").into(),
                        label.clone().into(),
                        strings.get(match method {
                            CredentialMethod::Key => "settings.providers.accounts.method.key",
                            CredentialMethod::OAuth => "settings.providers.accounts.method.oauth",
                            CredentialMethod::External => {
                                "settings.providers.accounts.method.external"
                            }
                        }),
                        Some(remove),
                    )
                }
                // A variable of the server's environment cannot be removed
                // from here.
                Connection::Environment { name, .. } => (
                    format!("providers-environment-{name}").into(),
                    name.clone().into(),
                    strings.get("settings.providers.accounts.environment"),
                    None,
                ),
            };
        let needs_auth = connection.needs_auth();
        div()
            .id(row_id.clone())
            .accessibility_id(row_id)
            .role(Role::Group)
            .aria_label(label.clone())
            .flex_row_from(direction)
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .py(px(6.0))
            .child(
                div().min_w_0().child(div().truncate().child(label)).child(
                    div()
                        .css_text(0.6875, 1.0, cx)
                        .text_color(if needs_auth {
                            palette.status_warning
                        } else {
                            palette.muted_foreground
                        })
                        .child(if needs_auth {
                            strings.get("settings.providers.card.status.reauthNeeded")
                        } else {
                            detail
                        }),
                ),
            )
            .children(remove)
            .test_support()
            .into_any_element()
    }

    /// One listed provider's accounts and models.
    fn provider_view(
        &mut self,
        provider: &ListedProvider,
        loaded: &Loaded,
        locked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let name: SharedString = provider
            .name
            .clone()
            .unwrap_or_else(|| provider.id.clone())
            .into();
        let header = self.sub_header(name, Some(&provider.id), View::List, cx);
        let held: Vec<Connection> = connections(&loaded.integrations, &provider.id)
            .unwrap_or_default()
            .into_iter()
            .cloned()
            .collect();
        let mut accounts: Vec<AnyElement> = held
            .iter()
            .map(|connection| self.account_row(connection, cx))
            .collect();
        if locked {
            // Stored accounts stay manageable; nothing new is added.
            accounts.push(
                div()
                    .id("providers-enterprise-line")
                    .child(helper(strings.get("settings.providers.enterpriseMode"), cx))
                    .test_support()
                    .into_any_element(),
            );
        } else {
            accounts.extend(self.add_methods(&provider.id, loaded, cx));
        }
        let mut children = vec![
            header,
            div()
                .mb(px(16.0))
                .css_text(0.8125, 1.25, cx)
                .text_color(palette.muted_foreground)
                .font_family(
                    gpui_kit::component::Theme::global(cx)
                        .mono_font_family
                        .clone(),
                )
                .child(SharedString::from(provider.id.clone()))
                .into_any_element(),
            section_with_info(
                "settings.providers.accounts.title",
                Some(strings.get("settings.providers.accounts.info")),
                true,
                accounts,
                Some(self.anchors[ACCOUNTS].clone()),
                cx,
            ),
        ];
        if provider.models.is_empty() {
            children.push(section_with_info(
                "settings.providers.page.models.title",
                None,
                false,
                vec![helper(
                    strings.get("settings.providers.page.models.none"),
                    cx,
                )],
                Some(self.anchors[MODELS].clone()),
                cx,
            ));
            return children;
        }
        let query = self.query(cx);
        let mut rows = vec![
            div()
                .flex_row_from(direction)
                .flex_wrap()
                .items_center()
                .gap(px(8.0))
                .child(self.search_field(
                    strings.get("settings.providers.page.models.filterPlaceholder"),
                    window,
                    cx,
                ))
                .child({
                    let id = provider.id.clone();
                    div().mb(px(12.0)).child(self.text_button(
                        "providers-hide-all".into(),
                        strings.get("settings.providers.page.actions.hideAll"),
                        true,
                        cx,
                        move |this, window, cx| this.set_all_hidden(&id, true, window, cx),
                    ))
                })
                .child({
                    let id = provider.id.clone();
                    div().mb(px(12.0)).child(self.text_button(
                        "providers-show-all".into(),
                        strings.get("settings.providers.page.actions.showAll"),
                        true,
                        cx,
                        move |this, window, cx| this.set_all_hidden(&id, false, window, cx),
                    ))
                })
                .into_any_element(),
        ];
        let matching: Vec<_> = provider
            .models
            .iter()
            .filter(|model| {
                query.is_empty()
                    || model.id.to_lowercase().contains(&query)
                    || model
                        .name
                        .as_deref()
                        .is_some_and(|name| name.to_lowercase().contains(&query))
            })
            .collect();
        if matching.is_empty() {
            rows.push(helper(
                strings.get("settings.providers.page.models.noModelsMatchFilter"),
                cx,
            ));
        }
        for model in matching {
            let hidden = loaded
                .hidden
                .iter()
                .any(|(held, id)| held == &provider.id && id == &model.id);
            let toggle_id: SharedString = format!("providers-model-toggle-{}", model.id).into();
            let focus = focus_handle(&mut self.focus, &toggle_id, cx);
            let hover = palette.hover;
            let (provider_id, model_id) = (provider.id.clone(), model.id.clone());
            let toggle = on_activate(
                div()
                    .id(toggle_id.clone())
                    .accessibility_id(toggle_id)
                    .role(Role::Button)
                    .aria_label(strings.get(if hidden {
                        "settings.providers.page.models.actions.showModel"
                    } else {
                        "settings.providers.page.models.actions.hideModel"
                    }))
                    .aria_toggled(if hidden {
                        Toggled::True
                    } else {
                        Toggled::False
                    })
                    .track_focus(&focus)
                    .tab_index(0)
                    .size(px(24.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .child(glyph(
                        if hidden { "eye-off" } else { "eye" },
                        14.0,
                        palette.muted_foreground,
                    )),
                cx,
                move |this, window, cx| this.toggle_hidden(&provider_id, &model_id, window, cx),
            )
            .test_support();
            let context = model.context_limit.map(|limit| {
                format!(
                    "{} {}",
                    limit,
                    strings.get("settings.providers.page.models.tokenBadge.context")
                )
            });
            rows.push(
                div()
                    .id(SharedString::from(format!("providers-model-{}", model.id)))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(12.0))
                    .py(px(4.0))
                    .when(hidden, |row| row.opacity(0.5))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(
                                model.name.clone().unwrap_or_else(|| model.id.clone()),
                            )),
                    )
                    .children(context.map(|context| {
                        div()
                            .flex_shrink_0()
                            .css_text(0.6875, 1.0, cx)
                            .text_color(palette.muted_foreground)
                            .child(SharedString::from(context))
                    }))
                    .child(toggle)
                    .test_support()
                    .into_any_element(),
            );
        }
        children.push(section_with_info(
            "settings.providers.page.models.title",
            None,
            false,
            rows,
            Some(self.anchors[MODELS].clone()),
            cx,
        ));
        children
    }

    /// A labelled field of the custom provider form with its error line.
    fn custom_field(
        item: &'static str,
        label: &'static str,
        info: Option<&'static str>,
        input: &Entity<InputState>,
        secret: bool,
        error: Option<&'static str>,
        enabled: bool,
        cx: &App,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let palette = cx.global::<Palette>();
        let direction = strings.direction();
        let id: SharedString = item.replace('.', "-").into();
        let mut field = Input::new(input)
            .id(id.clone())
            .accessibility_id(id)
            .aria_label(strings.get(label))
            .disabled(!enabled);
        if secret {
            field = field.content_type(InputContentType::Password);
        }
        div()
            .py(px(6.0))
            .child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .child(strings.get(label))
                    .children(info.map(|info| info_hint(item, strings.get(info), cx))),
            )
            .child(div().mt(px(6.0)).max_w(px(420.0)).child(field))
            .children(error.map(|error| {
                div()
                    .mt(px(4.0))
                    .css_text(0.75, 1.0, cx)
                    .text_color(palette.status_error)
                    .child(strings.get(error))
            }))
            .into_any_element()
    }

    /// The custom provider form (`CustomProviderForm`, create mode).
    fn custom_view(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let header = self.sub_header(
            strings.get("settings.providers.page.custom.title"),
            None,
            View::Connect,
            cx,
        );
        let Some(inputs) = &self.inputs else {
            return vec![header];
        };
        let (custom_id, custom_name, custom_url, custom_key) = (
            inputs.custom_id.clone(),
            inputs.custom_name.clone(),
            inputs.custom_url.clone(),
            inputs.custom_key.clone(),
        );
        let model_inputs: Vec<(Entity<InputState>, Entity<InputState>)> = inputs
            .models
            .iter()
            .map(|row| (row.id.clone(), row.name.clone()))
            .collect();
        let errors = self.custom_errors.clone();
        let enabled = !self.saving_custom && !self.writes.closing();
        let mut protocols = div()
            .id("providers-custom-protocol")
            .role(Role::Group)
            .aria_label(strings.get("settings.providers.page.custom.field.protocol.label"))
            .flex_row_from(direction)
            .flex_wrap()
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(px(7.0))
            .bg(palette.elevated);
        for (protocol, id, label) in [
            (
                CustomProtocol::OpenAiChat,
                "providers-custom-protocol-openai-chat",
                "settings.providers.page.custom.field.protocol.openaiChat",
            ),
            (
                CustomProtocol::OpenAiResponses,
                "providers-custom-protocol-openai-responses",
                "settings.providers.page.custom.field.protocol.openaiResponses",
            ),
            (
                CustomProtocol::AnthropicMessages,
                "providers-custom-protocol-anthropic-messages",
                "settings.providers.page.custom.field.protocol.anthropicMessages",
            ),
        ] {
            let selected = self.custom_protocol == protocol;
            let id: SharedString = id.into();
            let focus = focus_handle(&mut self.focus, &id, cx);
            let hover = palette.hover;
            protocols = protocols.child(
                on_activate(
                    div()
                        .id(id.clone())
                        .accessibility_id(id)
                        .role(Role::Button)
                        .aria_label(strings.get(label))
                        .aria_toggled(if selected {
                            Toggled::True
                        } else {
                            Toggled::False
                        })
                        .track_focus(&focus)
                        .tab_index(0)
                        .h(gpui_kit::rems(1.75))
                        .px(px(10.0))
                        .rounded(px(5.0))
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .when(selected, |chip| {
                            chip.bg(palette.background).font_weight(FontWeight::MEDIUM)
                        })
                        .when(!selected, |chip| chip.hover(move |style| style.bg(hover)))
                        .child(strings.get(label)),
                    cx,
                    move |this, _, cx| {
                        this.custom_protocol = protocol;
                        cx.notify();
                    },
                )
                .test_support(),
            );
        }
        let mut fields = vec![
            helper(
                strings.get("settings.providers.page.custom.description"),
                cx,
            ),
            Self::custom_field(
                "providers.custom.id",
                "settings.providers.page.custom.field.providerID.label",
                Some("settings.providers.page.custom.field.providerID.info"),
                &custom_id,
                false,
                errors.id,
                enabled,
                cx,
            ),
            Self::custom_field(
                "providers.custom.name",
                "settings.providers.page.custom.field.name.label",
                Some("settings.providers.page.custom.field.name.info"),
                &custom_name,
                false,
                errors.name,
                enabled,
                cx,
            ),
            div()
                .py(px(6.0))
                .child(strings.get("settings.providers.page.custom.field.protocol.label"))
                .child(div().mt(px(6.0)).flex().child(protocols))
                .into_any_element(),
            Self::custom_field(
                "providers.custom.url",
                "settings.providers.page.custom.field.baseURL.label",
                Some("settings.providers.page.custom.field.baseURL.info"),
                &custom_url,
                false,
                errors.base_url,
                enabled,
                cx,
            ),
            Self::custom_field(
                "providers.custom.key",
                "settings.providers.page.custom.field.apiKey.label",
                Some("settings.providers.page.custom.field.apiKey.info"),
                &custom_key,
                true,
                errors.api_key,
                enabled,
                cx,
            ),
        ];
        if self.custom_persisted.is_some() {
            fields.push(
                div()
                    .id("providers-custom-key-hint")
                    .css_text(0.8125, 1.25, cx)
                    .text_color(palette.status_warning)
                    .child(strings.get("settings.providers.page.custom.authFailure.keyAfterConfig"))
                    .test_support()
                    .into_any_element(),
            );
        }
        let removable = model_inputs.len() > 1;
        let mut models = Vec::new();
        for (index, (id, name)) in model_inputs.iter().enumerate() {
            let (id_error, name_error) = errors.models.get(index).copied().unwrap_or_default();
            let remove = removable.then(|| {
                self.text_button(
                    format!("providers-custom-remove-model-{index}").into(),
                    strings.get("settings.providers.page.custom.models.remove"),
                    enabled,
                    cx,
                    move |this, _, cx| this.remove_model(index, cx),
                )
            });
            let cell = |input: &Entity<InputState>,
                        id: String,
                        label: &'static str,
                        error: Option<&'static str>| {
                div()
                    .w(px(220.0))
                    .child(
                        Input::new(input)
                            .id(SharedString::from(id.clone()))
                            .accessibility_id(SharedString::from(id))
                            .aria_label(strings.get(label))
                            .disabled(!enabled),
                    )
                    .children(error.map(|error| {
                        div()
                            .mt(px(4.0))
                            .css_text(0.75, 1.0, cx)
                            .text_color(palette.status_error)
                            .child(strings.get(error))
                    }))
            };
            models.push(
                div()
                    .flex_row_from(direction)
                    .flex_wrap()
                    .items_start()
                    .gap(px(8.0))
                    .py(px(4.0))
                    .child(cell(
                        id,
                        format!("providers-custom-model-id-{index}"),
                        "settings.providers.page.custom.models.idLabel",
                        id_error,
                    ))
                    .child(cell(
                        name,
                        format!("providers-custom-model-name-{index}"),
                        "settings.providers.page.custom.models.nameLabel",
                        name_error,
                    ))
                    .children(remove)
                    .into_any_element(),
            );
        }
        models.push(
            div()
                .flex()
                .child(self.text_button(
                    "providers-custom-add-model".into(),
                    strings.get("settings.providers.page.custom.models.add"),
                    enabled,
                    cx,
                    |this, window, cx| this.add_model(window, cx),
                ))
                .into_any_element(),
        );
        let saving = self.saving_custom;
        let save = self.text_button(
            "providers-custom-save".into(),
            strings.get(if saving {
                "settings.providers.page.actions.saving"
            } else {
                "settings.providers.page.custom.actions.save"
            }),
            !saving,
            cx,
            |this, window, cx| this.save_custom(window, cx),
        );
        let cancel = self.text_button(
            "providers-custom-cancel".into(),
            strings.get("settings.providers.page.actions.cancel"),
            !saving,
            cx,
            |this, window, cx| {
                this.custom_persisted = None;
                this.reset_custom(window, cx);
                this.open(View::Connect, window, cx);
            },
        );
        vec![
            header,
            section_with_info(
                "settings.providers.page.custom.title",
                None,
                true,
                fields,
                None,
                cx,
            ),
            section_with_info(
                "settings.providers.page.custom.models.title",
                None,
                false,
                models,
                None,
                cx,
            ),
            div()
                .mt(px(24.0))
                .flex_row_from(direction)
                .gap(px(8.0))
                .child(save)
                .child(cancel)
                .into_any_element(),
        ]
    }
}

impl Render for ProvidersPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.retry_requested) {
            self.retry(window, cx);
        }
        let strings = locale::strings(cx).clone();
        let frame = page_frame("providers-settings-page", Page::Providers, &self.scroll, cx);
        let loaded = match (&self.target, &self.load) {
            (None, _) => {
                return frame.child(helper(strings.get("common.unavailable"), cx));
            }
            (_, Load::Loading) => return frame,
            (_, Load::Failed) => {
                return frame.child(load_failed(
                    "providers-settings-load-failed",
                    strings.get("settings.providers.page.state.unableToLoadProviderList"),
                    strings.get("desktopHostSwitcher.actions.retry"),
                    cx,
                    |this, cx| this.refresh(cx),
                ));
            }
            (_, Load::Ready(loaded)) => loaded.clone(),
        };
        self.ensure_inputs(window, cx);
        let locked = self.locked(cx);
        // Enterprise mode has no connect view, whatever opened one.
        if locked && matches!(self.view, View::Connect | View::Candidate(_) | View::Custom) {
            self.view = View::List;
        }
        let failed = (!self.failed.is_empty()).then(|| {
            load_failed(
                "providers-settings-save-failed",
                strings.get("settings.common.status.saveFailed"),
                strings.get("desktopHostSwitcher.actions.retry"),
                cx,
                |this, cx| {
                    this.retry_requested = true;
                    cx.notify();
                },
            )
        });
        let children = match self.view.clone() {
            View::Provider(id) => {
                match loaded.providers.iter().find(|provider| provider.id == id) {
                    Some(provider) => self.provider_view(provider, &loaded, locked, window, cx),
                    // OpenCode does not list it yet, or no longer does.
                    None => self.list_view(&loaded, locked, window, cx),
                }
            }
            View::List => self.list_view(&loaded, locked, window, cx),
            View::Connect => self.connect_view(&loaded, window, cx),
            View::Candidate(id) => self.candidate_view(&id, &loaded, cx),
            View::Custom => self.custom_view(cx),
        };
        frame
            .children(failed.map(|failed| div().mb(px(16.0)).child(failed)))
            .children(children)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, Entity, TestAppContext};
    use serde_json::{Value, json};

    use super::*;
    use crate::enterprise::PolicyStatus;
    use crate::settings_controls::tests::{ResponseGate, retarget};
    use crate::settings_test_server::{Server, open, settings, settle};

    const KEY: &str = "sk-fixture-never-shown";

    struct Fixture {
        server: Server,
        /// The integrations as the server lists them.
        integrations: Arc<Mutex<Vec<Value>>>,
        providers: Arc<Mutex<Vec<Value>>>,
        document: Arc<Mutex<Value>>,
        policy: Arc<Mutex<Value>>,
        /// What the next attempt status read answers.
        attempt_status: Arc<Mutex<Value>>,
        fail_reads: Arc<AtomicBool>,
        fail_writes: Arc<AtomicBool>,
        /// The server refuses ways in as enterprise mode.
        refuse_entry: Arc<AtomicBool>,
    }

    fn integration(id: &str, name: &str, connections: Value) -> Value {
        json!({
            "id": id,
            "name": name,
            "methods": [
                {"type": "key"},
                {"type": "oauth", "id": "browser", "label": "Sign in with browser"},
            ],
            "connections": connections,
        })
    }

    /// A server that lists `provider` (connected with one stored key) and
    /// can still connect `openrouter`.
    fn fixture(provider: &'static str) -> Fixture {
        gated_fixture(provider, None)
    }

    /// `gate` holds every request that is not a read.
    fn gated_fixture(provider: &'static str, gate: Option<Arc<ResponseGate>>) -> Fixture {
        let integrations = Arc::new(Mutex::new(vec![
            integration(
                provider,
                "Anthropic",
                json!([{"type": "credential", "id": "cred_1", "label": "Work", "method": "key"}]),
            ),
            integration("openrouter", "OpenRouter", json!([])),
            integration("mcp_linear", "Linear", json!([])),
        ]));
        let providers = Arc::new(Mutex::new(vec![
            json!({"id": provider, "name": "Anthropic"}),
        ]));
        let document = Arc::new(Mutex::new(json!({"hiddenModels": []})));
        let policy = Arc::new(Mutex::new(json!({
            "enterpriseMode": false, "source": null, "organization": null, "policyError": null,
        })));
        let attempt_status = Arc::new(Mutex::new(json!({"status": "pending"})));
        let fail_reads = Arc::new(AtomicBool::new(false));
        let fail_writes = Arc::new(AtomicBool::new(false));
        let refuse_entry = Arc::new(AtomicBool::new(false));
        let (listed, held, stored, rules, status) = (
            Arc::clone(&integrations),
            Arc::clone(&providers),
            Arc::clone(&document),
            Arc::clone(&policy),
            Arc::clone(&attempt_status),
        );
        let (reads, writes, refuse) = (
            Arc::clone(&fail_reads),
            Arc::clone(&fail_writes),
            Arc::clone(&refuse_entry),
        );
        let server = Server::new(move |request| {
            let path = request.target.split('?').next().unwrap_or_default();
            let located = |data: Value| (200, json!({"location": {}, "data": data}));
            if request.method == "GET" {
                if reads.load(Ordering::Acquire) {
                    return (500, json!({"error": "fixture read failure"}));
                }
                return match path {
                    "/api/provider" => {
                        located(Value::Array(held.lock().expect("providers").clone()))
                    }
                    "/api/model" => {
                        let models: Vec<Value> = held
                            .lock()
                            .expect("providers")
                            .iter()
                            .flat_map(|provider| {
                                ["sonnet", "haiku"].map(|model| {
                                    json!({
                                        "id": model, "modelID": model,
                                        "providerID": provider["id"], "name": model,
                                        "enabled": true,
                                    })
                                })
                            })
                            .collect();
                        located(Value::Array(models))
                    }
                    "/api/model/default" => located(Value::Null),
                    "/api/integration" => {
                        located(Value::Array(listed.lock().expect("integrations").clone()))
                    }
                    "/api/openchamber/enterprise-policy" => {
                        (200, rules.lock().expect("policy").clone())
                    }
                    "/api/config/settings" => (200, stored.lock().expect("document").clone()),
                    path if path.contains("/connect/oauth/") => {
                        located(status.lock().expect("status").clone())
                    }
                    _ => (404, json!({"error": "no such fixture route"})),
                };
            }
            if let Some(gate) = &gate {
                gate.hold();
            }
            let entry = request.method == "POST" || path == "/api/provider";
            if entry && refuse.load(Ordering::Acquire) {
                return (
                    403,
                    json!({"error": "Not available in enterprise mode", "code": "enterprise_mode"}),
                );
            }
            if writes.load(Ordering::Acquire) {
                // A server that quotes the request it refused.
                return (
                    500,
                    json!({"error": format!("fixture write failure for {}", request.body)}),
                );
            }
            let mut integrations = listed.lock().expect("integrations");
            if request.method == "DELETE" && path.starts_with("/api/credential/") {
                let id = path.trim_start_matches("/api/credential/").to_owned();
                for integration in integrations.iter_mut() {
                    if let Some(connections) = integration["connections"].as_array_mut() {
                        connections.retain(|connection| connection["id"] != id.as_str());
                    }
                }
                return (200, json!({}));
            }
            if request.method == "PUT" && path == "/api/provider" {
                let id = request.body["providerID"].clone();
                held.lock()
                    .expect("providers")
                    .push(json!({"id": id, "name": request.body["config"]["name"]}));
                return (200, json!({"success": true}));
            }
            if request.method == "PUT" {
                let mut document = stored.lock().expect("document");
                if let Some(changes) = request.body.as_object() {
                    for (key, value) in changes {
                        document[key.as_str()] = value.clone();
                    }
                }
                return (200, document.clone());
            }
            if path.ends_with("/connect/oauth") {
                return located(json!({
                    "attemptID": "att_1",
                    "url": "https://auth.example.test/device",
                    "instructions": "Enter code: ABCD-1234",
                    "mode": if request.body["methodID"] == "paste" { "code" } else { "auto" },
                }));
            }
            if path.ends_with("/connect/key") || path.ends_with("/complete") {
                let id = path.split('/').nth(3).unwrap_or_default().to_owned();
                if let Some(integration) = integrations
                    .iter_mut()
                    .find(|integration| integration["id"] == id.as_str())
                    && let Some(connections) = integration["connections"].as_array_mut()
                {
                    connections.push(json!({
                        "type": "credential", "id": format!("cred_{id}"), "label": id,
                        "method": "key",
                    }));
                }
                if !held
                    .lock()
                    .expect("providers")
                    .iter()
                    .any(|provider| provider["id"] == id.as_str())
                {
                    held.lock()
                        .expect("providers")
                        .push(json!({"id": id, "name": id}));
                }
            }
            (200, json!({}))
        });
        Fixture {
            server,
            integrations,
            providers,
            document,
            policy,
            attempt_status,
            fail_reads,
            fail_writes,
            refuse_entry,
        }
    }

    /// The machine policy is unread in a test, which counts as enterprise
    /// mode; this is a machine without one.
    fn unlock(workspace: &Entity<Workspace>, cx: &mut TestAppContext) {
        workspace.update(cx, |workspace, cx| {
            workspace.enterprise.status = PolicyStatus {
                enabled: false,
                unavailable: false,
            };
            cx.notify();
        });
    }

    fn opened(
        fixture: &Fixture,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>, Entity<ProvidersPage>) {
        let (window, workspace) = open(&fixture.server, Page::Providers, cx);
        unlock(&workspace, cx);
        settle(window, cx);
        let settings = settings(&workspace, cx);
        let page = cx.read(|cx| settings.read(cx).providers.clone());
        (window, workspace, page)
    }

    fn act(
        window: AnyWindowHandle,
        page: &Entity<ProvidersPage>,
        cx: &mut TestAppContext,
        action: impl FnOnce(&mut ProvidersPage, &mut Window, &mut Context<ProvidersPage>),
    ) {
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| action(page, window, cx));
        })
        .expect("test window");
        settle(window, cx);
    }

    fn loaded(page: &Entity<ProvidersPage>, cx: &TestAppContext) -> Loaded {
        cx.read(|cx| page.read(cx).load.ready().cloned().expect("loaded"))
    }

    fn notice(page: &Entity<ProvidersPage>, cx: &TestAppContext) -> String {
        cx.read(|cx| {
            page.read(cx)
                .notice
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default()
        })
    }

    /// Types `text` into the key field of the open view.
    fn type_key(
        window: AnyWindowHandle,
        page: &Entity<ProvidersPage>,
        text: &str,
        cx: &mut TestAppContext,
    ) {
        act(window, page, cx, |page, window, cx| {
            let key = page.inputs.as_ref().expect("inputs").key.clone();
            key.update(cx, |input, cx| input.set_value(text, window, cx));
        });
    }

    fn key_text(page: &Entity<ProvidersPage>, cx: &TestAppContext) -> String {
        cx.read(|cx| {
            page.read(cx)
                .inputs
                .as_ref()
                .map(|inputs| inputs.key.read(cx).value().to_string())
                .unwrap_or_default()
        })
    }

    fn count(fixture: &Fixture, method: &str, prefix: &str) -> usize {
        fixture
            .server
            .requests()
            .iter()
            .filter(|request| request.method == method && request.target.starts_with(prefix))
            .count()
    }

    #[test]
    fn statuses_and_candidates_follow_the_integrations() {
        let integrations: Vec<Integration> = [
            ("anthropic", vec![("cred_1", false), ("cred_2", false)]),
            ("openai", vec![("cred_3", true)]),
            ("openrouter", vec![]),
            ("mcp_linear", vec![]),
        ]
        .into_iter()
        .map(|(id, credentials)| Integration {
            id: id.to_owned(),
            name: id.to_owned(),
            accepts_key: true,
            key_needs_answers: false,
            sign_in: Vec::new(),
            connections: credentials
                .into_iter()
                .map(|(id, needs_auth)| Connection::Credential {
                    id: id.to_owned(),
                    label: id.to_owned(),
                    method: CredentialMethod::Key,
                    needs_auth,
                })
                .collect(),
            web_search: false,
        })
        .collect();
        let provider = |id: &str, models: usize| ListedProvider {
            id: id.to_owned(),
            name: None,
            models: (0..models)
                .map(
                    |index| openchamber_native_transport::providers::ListedModel {
                        id: index.to_string(),
                        name: None,
                        context_limit: None,
                    },
                )
                .collect(),
        };
        assert_eq!(
            card_status(&integrations, &provider("anthropic", 1)),
            Some(CardStatus::Accounts(2))
        );
        assert_eq!(
            card_status(&integrations, &provider("openai", 1)),
            Some(CardStatus::ReauthNeeded)
        );
        assert_eq!(
            card_status(&integrations, &provider("openrouter", 1)),
            Some(CardStatus::SignInNeeded)
        );
        assert_eq!(
            card_status(&integrations, &provider("openrouter", 0)),
            Some(CardStatus::NoModels)
        );
        assert_eq!(
            card_status(&integrations, &provider("from-config", 1)),
            None,
            "a provider without an integration is not called signed out"
        );
        let listed = HashSet::from(["anthropic", "openai"]);
        let candidates: Vec<&str> = connectable(&integrations, &listed)
            .into_iter()
            .map(|integration| integration.id.as_str())
            .collect();
        assert_eq!(candidates, ["openrouter"], "no MCP sign-in, nothing listed");
        assert_eq!(user_code("Enter code: ABCD-1234 now"), Some("ABCD-1234"));
        assert_eq!(user_code("Enter code: ABCD-12345"), Some("ABCD-12345"));
        assert_eq!(user_code("no code here"), None);
        assert!(opens_in_browser("https://auth.example.test/device"));
        assert!(!opens_in_browser("file:///etc/passwd"));
        assert!(!opens_in_browser("javascript:alert(1)"));
    }

    #[test]
    fn a_custom_provider_is_checked_field_by_field() {
        let existing = HashSet::from(["anthropic"]);
        let draft = |id: &str, url: &str, models: &[(&str, &str)]| CustomDraft {
            id: id.to_owned(),
            name: "Local".to_owned(),
            base_url: url.to_owned(),
            protocol: CustomProtocol::OpenAiChat,
            models: models
                .iter()
                .map(|(id, name)| ((*id).to_owned(), (*name).to_owned()))
                .collect(),
        };
        let errors = validate_custom(
            &draft("Bad ID", "ftp://host", &[("m", "M"), ("m", "")]),
            &DraftCredential::Missing,
            &existing,
            None,
        )
        .expect_err("invalid");
        assert_eq!(
            errors.id,
            Some("settings.providers.page.custom.error.providerID.format")
        );
        assert_eq!(
            errors.base_url,
            Some("settings.providers.page.custom.error.baseURL.format")
        );
        assert_eq!(
            errors.api_key,
            Some("settings.providers.page.custom.error.apiKey.required")
        );
        assert_eq!(
            errors.models[1],
            (
                Some("settings.providers.page.custom.error.duplicate"),
                Some("settings.providers.page.custom.error.required")
            )
        );
        let taken = validate_custom(
            &draft("anthropic", "https://host/v1", &[("m", "M")]),
            &DraftCredential::Key,
            &existing,
            None,
        )
        .expect_err("taken");
        assert_eq!(
            taken.id,
            Some("settings.providers.page.custom.error.providerID.exists")
        );
        let (credential, key) = draft_credential(" {env: LOCAL_KEY } ");
        assert_eq!(credential, DraftCredential::Env("LOCAL_KEY".to_owned()));
        assert!(key.is_none(), "a variable name is not a key");
        let (literal, key) = draft_credential(KEY);
        assert_eq!(literal, DraftCredential::Key);
        assert!(key.is_some());
        let provider = validate_custom(
            &draft(
                " local ",
                " http://localhost:11434/v1 ",
                &[(" llama ", " Llama ")],
            ),
            &credential,
            &existing,
            None,
        )
        .expect("valid");
        assert_eq!(provider.id, "local");
        assert_eq!(provider.env.as_deref(), Some("LOCAL_KEY"));
        assert_eq!(provider.models, [("llama".to_owned(), "Llama".to_owned())]);
        assert!(!provider.has_credential);
        assert!(
            validate_custom(
                &draft("anthropic", "https://host/v1", &[("m", "M")]),
                &DraftCredential::Key,
                &existing,
                Some("anthropic"),
            )
            .is_ok(),
            "the provider an earlier save of this form wrote may be written again"
        );
    }

    #[gpui_kit::test]
    fn providers_load_with_their_connection_state(cx: &mut TestAppContext) {
        let fixture = fixture("anthropic");
        let (window, _, page) = opened(&fixture, cx);
        let loaded = loaded(&page, cx);
        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(loaded.integrations.len(), 3);
        assert_eq!(
            card_status(&loaded.integrations, &loaded.providers[0]),
            Some(CardStatus::Connected)
        );
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("providers-row-anthropic").is_some());
            assert!(window.try_find("providers-connect").is_some());
            assert!(window.try_find("providers-enterprise-notice").is_none());
            assert!(window.try_find("providers-empty").is_none());
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn a_failed_read_offers_retry_and_no_empty_list(cx: &mut TestAppContext) {
        let fixture = fixture("anthropic");
        fixture.fail_reads.store(true, Ordering::Release);
        let (window, _, page) = opened(&fixture, cx);
        cx.read(|cx| assert_eq!(page.read(cx).load, Load::Failed));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window
                    .try_find("providers-settings-load-failed-retry")
                    .is_some()
            );
            assert!(window.try_find("providers-empty").is_none());
            assert!(
                window.try_find("providers-connect").is_none(),
                "nothing connects before the policy is known"
            );
        })
        .expect("test window");
        fixture.fail_reads.store(false, Ordering::Release);
        act(window, &page, cx, |page, _, cx| page.refresh(cx));
        assert_eq!(loaded(&page, cx).providers.len(), 1);
    }

    #[gpui_kit::test]
    fn a_saved_key_empties_its_field_and_connects_the_provider(cx: &mut TestAppContext) {
        let fixture = fixture("anthropic");
        let (window, _, page) = opened(&fixture, cx);
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Candidate("openrouter".into()), window, cx);
        });
        type_key(window, &page, &format!("  {KEY} "), cx);
        act(window, &page, cx, |page, window, cx| {
            page.connect_key(window, cx);
        });
        let writes = fixture.server.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].target, "/api/integration/openrouter/connect/key");
        assert_eq!(writes[0].body, json!({ "key": KEY }));
        assert_eq!(
            key_text(&page, cx),
            "",
            "the field holds no key after a save"
        );
        assert_eq!(notice(&page, cx), "API key saved");
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.view, View::Provider("openrouter".into()));
            assert!(page.connecting_keys.is_empty());
        });
        let loaded = loaded(&page, cx);
        assert!(
            loaded
                .providers
                .iter()
                .any(|provider| provider.id == "openrouter"),
            "the list was read again after the change"
        );
    }

    #[gpui_kit::test]
    fn a_failed_key_save_keeps_the_field_and_never_shows_the_key(cx: &mut TestAppContext) {
        let fixture = fixture("anthropic");
        let (window, _, page) = opened(&fixture, cx);
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Candidate("openrouter".into()), window, cx);
        });
        type_key(window, &page, KEY, cx);
        fixture.fail_writes.store(true, Ordering::Release);
        act(window, &page, cx, |page, window, cx| {
            page.connect_key(window, cx);
        });
        let shown = notice(&page, cx);
        assert_eq!(shown, "Failed to save API key");
        assert!(
            !shown.contains(KEY),
            "the server quoted the key and the page did not repeat it"
        );
        assert_eq!(key_text(&page, cx), KEY, "Save can be pressed again");
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.view, View::Candidate("openrouter".into()));
            assert!(
                !page.has_failed_writes(),
                "no key is kept anywhere for a retry"
            );
        });
        // The field is a secret input: it exposes no value.
        cx.update_window(window, |_, window, cx| {
            let key = page.read(cx).inputs.as_ref().expect("inputs").key.clone();
            let mut node = gpui_kit::accesskit::Node::new(Role::TextInput);
            key.update(cx, |input, cx| {
                input
                    .render(window, cx)
                    .into_element()
                    .write_a11y_info(&mut node);
            });
            assert!(
                node.value().is_none(),
                "the key field never exposes its text"
            );
        })
        .expect("test window");
        // Leaving the view drops what the field held.
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Connect, window, cx);
        });
        assert_eq!(key_text(&page, cx), "");
        // An empty key is refused before any request.
        fixture.fail_writes.store(false, Ordering::Release);
        let before = fixture.server.writes().len();
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Candidate("openrouter".into()), window, cx);
            page.connect_key(window, cx);
        });
        assert_eq!(fixture.server.writes().len(), before);
        assert_eq!(notice(&page, cx), "API key is required");
    }

    #[gpui_kit::test]
    fn a_key_save_in_flight_refuses_a_second_one(cx: &mut TestAppContext) {
        let gate = ResponseGate::new();
        let fixture = gated_fixture("anthropic", Some(Arc::clone(&gate)));
        let (window, _, page) = opened(&fixture, cx);
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Candidate("openrouter".into()), window, cx);
        });
        type_key(window, &page, KEY, cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.connect_key(window, cx);
                page.connect_key(window, cx);
            });
        })
        .expect("test window");
        gate.entered(cx);
        gate.release();
        settle(window, cx);
        assert_eq!(
            fixture.server.writes().len(),
            1,
            "one write for two presses"
        );
    }

    #[gpui_kit::test]
    fn removing_an_account_disconnects_and_a_failed_removal_keeps_it(cx: &mut TestAppContext) {
        let fixture = fixture("anthropic");
        let (window, _, page) = opened(&fixture, cx);
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Provider("anthropic".into()), window, cx);
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("providers-account-cred_1").is_some());
        })
        .expect("test window");
        fixture.fail_writes.store(true, Ordering::Release);
        act(window, &page, cx, |page, window, cx| {
            page.remove_credential("cred_1".into(), "Work".into(), window, cx);
        });
        assert_eq!(notice(&page, cx), "Couldn’t remove the account");
        assert_eq!(
            loaded(&page, cx).integrations[0].credentials().count(),
            1,
            "the account stays"
        );
        cx.read(|cx| assert!(page.read(cx).has_failed_writes()));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window
                    .try_find("providers-settings-save-failed-retry")
                    .is_some()
            );
        })
        .expect("test window");
        fixture.fail_writes.store(false, Ordering::Release);
        act(window, &page, cx, |page, window, cx| page.retry(window, cx));
        assert_eq!(notice(&page, cx), "Account removed");
        assert_eq!(loaded(&page, cx).integrations[0].credentials().count(), 0);
        cx.read(|cx| {
            let page = page.read(cx);
            assert!(!page.has_failed_writes());
            assert!(page.removing.is_empty());
        });
        assert_eq!(
            count(&fixture, "DELETE", "/api/credential/cred_1"),
            2,
            "the failed removal and its retry"
        );
    }

    #[gpui_kit::test]
    fn a_custom_provider_is_validated_then_saved_with_its_key_after_the_config(
        cx: &mut TestAppContext,
    ) {
        let fixture = fixture("anthropic");
        let (window, _, page) = opened(&fixture, cx);
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Custom, window, cx);
        });
        act(window, &page, cx, |page, window, cx| {
            page.save_custom(window, cx);
        });
        assert!(
            fixture.server.writes().is_empty(),
            "an invalid form sends nothing"
        );
        cx.read(|cx| {
            let errors = &page.read(cx).custom_errors;
            assert!(errors.id.is_some() && errors.base_url.is_some() && errors.api_key.is_some());
        });
        act(window, &page, cx, |page, window, cx| {
            let inputs = page.inputs.as_ref().expect("inputs");
            let fields = [
                (inputs.custom_id.clone(), "local"),
                (inputs.custom_name.clone(), "Local"),
                (inputs.custom_url.clone(), "http://localhost:11434/v1"),
                (inputs.custom_key.clone(), KEY),
                (inputs.models[0].id.clone(), "llama"),
                (inputs.models[0].name.clone(), "Llama"),
            ];
            for (field, text) in fields {
                field.update(cx, |input, cx| input.set_value(text, window, cx));
            }
        });
        act(window, &page, cx, |page, window, cx| {
            page.save_custom(window, cx);
        });
        let writes = fixture.server.writes();
        assert_eq!(writes.len(), 2);
        assert!(writes[0].is("PUT", "/api/provider"));
        assert_eq!(writes[0].body["providerID"], "local");
        assert_eq!(writes[0].body["hasCredential"], true);
        assert!(
            !writes[0].body.to_string().contains(KEY),
            "the config write carries no key"
        );
        assert_eq!(writes[1].target, "/api/integration/local/connect/key");
        assert_eq!(writes[1].body, json!({ "key": KEY }));
        assert_eq!(notice(&page, cx), "Local connected");
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.view, View::Provider("local".into()));
            let inputs = page.inputs.as_ref().expect("inputs");
            assert_eq!(inputs.custom_key.read(cx).value().to_string(), "");
            assert_eq!(inputs.custom_id.read(cx).value().to_string(), "");
        });
    }

    #[gpui_kit::test]
    fn a_browser_sign_in_is_polled_to_completion(cx: &mut TestAppContext) {
        let fixture = fixture("anthropic");
        let (window, _, page) = opened(&fixture, cx);
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Candidate("openrouter".into()), window, cx);
        });
        act(window, &page, cx, |page, window, cx| {
            page.start_sign_in("browser".into(), window, cx);
        });
        cx.read(|cx| {
            let page = page.read(cx);
            assert!(matches!(page.flow, Flow::Waiting { .. }));
            assert_eq!(page.opened, ["https://auth.example.test/device"]);
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("providers-flow").is_some());
            assert!(window.try_find("providers-flow-device-code").is_some());
        })
        .expect("test window");
        // Still pending: the attempt is asked again.
        cx.executor().advance_clock(POLL_INTERVAL);
        settle(window, cx);
        cx.read(|cx| assert!(matches!(page.read(cx).flow, Flow::Waiting { .. })));
        *fixture.attempt_status.lock().expect("status") = json!({"status": "complete"});
        fixture.integrations.lock().expect("integrations")[1]["connections"] =
            json!([{"type": "credential", "id": "cred_2", "label": "Me", "method": "oauth"}]);
        fixture
            .providers
            .lock()
            .expect("providers")
            .push(json!({"id": "openrouter", "name": "OpenRouter"}));
        cx.executor().advance_clock(POLL_INTERVAL);
        settle(window, cx);
        assert_eq!(notice(&page, cx), "OAuth connection completed");
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.flow, Flow::Idle);
            assert_eq!(page.view, View::Provider("openrouter".into()));
        });
        assert_eq!(
            card_status(
                &loaded(&page, cx).integrations,
                &loaded(&page, cx).providers[1]
            ),
            Some(CardStatus::Connected)
        );
    }

    #[gpui_kit::test]
    fn a_failed_attempt_ends_the_sign_in_and_leaving_cancels_one(cx: &mut TestAppContext) {
        let fixture = fixture("anthropic");
        let (window, _, page) = opened(&fixture, cx);
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Candidate("openrouter".into()), window, cx);
        });
        act(window, &page, cx, |page, window, cx| {
            page.start_sign_in("browser".into(), window, cx);
        });
        *fixture.attempt_status.lock().expect("status") =
            json!({"status": "failed", "message": "declined in the browser"});
        cx.executor().advance_clock(POLL_INTERVAL);
        settle(window, cx);
        cx.read(|cx| {
            assert!(matches!(
                &page.read(cx).flow,
                Flow::Failed { message, .. } if message.as_ref() == "declined in the browser"
            ));
        });
        assert_eq!(
            count(&fixture, "DELETE", "/api/integration/"),
            0,
            "a finished attempt has nothing to cancel"
        );
        // A second sign-in, left by going back.
        *fixture.attempt_status.lock().expect("status") = json!({"status": "pending"});
        act(window, &page, cx, |page, window, cx| {
            page.start_sign_in("browser".into(), window, cx);
        });
        cx.read(|cx| assert!(matches!(page.read(cx).flow, Flow::Waiting { .. })));
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Connect, window, cx);
        });
        cx.read(|cx| assert_eq!(page.read(cx).flow, Flow::Idle));
        assert_eq!(
            count(
                &fixture,
                "DELETE",
                "/api/integration/openrouter/connect/oauth/att_1"
            ),
            1,
            "the abandoned attempt is dropped upstream"
        );
        let polls = count(
            &fixture,
            "GET",
            "/api/integration/openrouter/connect/oauth/",
        );
        cx.executor().advance_clock(POLL_INTERVAL * 3);
        settle(window, cx);
        assert_eq!(
            count(
                &fixture,
                "GET",
                "/api/integration/openrouter/connect/oauth/"
            ),
            polls,
            "nothing polls an abandoned attempt"
        );
    }

    #[gpui_kit::test]
    fn enterprise_mode_shows_who_manages_and_refuses_every_way_in(cx: &mut TestAppContext) {
        let fixture = fixture("anthropic");
        *fixture.policy.lock().expect("policy") = json!({
            "enterpriseMode": true, "source": "policy-file", "organization": "Acme",
            "policyError": null,
        });
        let (window, _, page) = opened(&fixture, cx);
        cx.read(|cx| assert!(page.read(cx).locked(cx)));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("providers-enterprise-notice").is_some());
            assert!(window.try_find("providers-enterprise-managed-by").is_some());
            assert!(window.try_find("providers-connect").is_none());
        })
        .expect("test window");
        // The connect views do not open, whatever asks for them.
        act(window, &page, cx, |page, window, cx| {
            page.reveal("providers.connect", window, cx);
            page.reveal("providers.custom", window, cx);
            page.open(View::Candidate("openrouter".into()), window, cx);
        });
        cx.read(|cx| assert_eq!(page.read(cx).view, View::List));
        // The provider's own page keeps its accounts and offers no key.
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Provider("anthropic".into()), window, cx);
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("providers-account-cred_1").is_some());
            assert!(window.try_find("providers-enterprise-line").is_some());
            assert!(window.try_find("providers-key-form").is_none());
            assert!(window.try_find("providers-sign-in-browser").is_none());
        })
        .expect("test window");
        // The functions refuse too, without a request.
        type_key(window, &page, KEY, cx);
        act(window, &page, cx, |page, window, cx| {
            page.connect_key(window, cx);
            page.start_sign_in("browser".into(), window, cx);
            page.save_custom(window, cx);
        });
        assert!(
            fixture.server.writes().is_empty(),
            "nothing reaches the server in enterprise mode"
        );
        assert!(notice(&page, cx).starts_with("Providers are managed in the OpenCode config"));
        // Removing an account narrows access and stays allowed.
        act(window, &page, cx, |page, window, cx| {
            page.remove_credential("cred_1".into(), "Work".into(), window, cx);
        });
        assert_eq!(count(&fixture, "DELETE", "/api/credential/cred_1"), 1);
        assert_eq!(notice(&page, cx), "Account removed");
    }

    #[gpui_kit::test]
    fn a_server_that_refuses_as_enterprise_mode_locks_the_page(cx: &mut TestAppContext) {
        let fixture = fixture("anthropic");
        let (window, _, page) = opened(&fixture, cx);
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Candidate("openrouter".into()), window, cx);
        });
        type_key(window, &page, KEY, cx);
        // The administrator turns the mode on after the page was read.
        fixture.refuse_entry.store(true, Ordering::Release);
        *fixture.policy.lock().expect("policy") = json!({
            "enterpriseMode": true, "source": "environment", "organization": null,
            "policyError": null,
        });
        act(window, &page, cx, |page, window, cx| {
            page.connect_key(window, cx);
        });
        assert!(notice(&page, cx).starts_with("Providers are managed in the OpenCode config"));
        assert_eq!(key_text(&page, cx), "", "a refused key is not kept");
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.view, View::List);
            assert!(page.locked(cx), "the policy was read again");
        });
    }

    #[gpui_kit::test]
    fn a_machine_policy_locks_the_page_whatever_the_server_says(cx: &mut TestAppContext) {
        let fixture = fixture("anthropic");
        let (window, workspace, page) = opened(&fixture, cx);
        cx.read(|cx| assert!(!page.read(cx).locked(cx)));
        workspace.update(cx, |workspace, cx| {
            workspace.enterprise.status = PolicyStatus {
                enabled: true,
                unavailable: false,
            };
            cx.notify();
        });
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Connect, window, cx);
        });
        cx.read(|cx| {
            let page = page.read(cx);
            assert!(page.locked(cx));
            assert_eq!(page.view, View::List);
        });
        assert!(fixture.server.writes().is_empty());
    }

    #[gpui_kit::test]
    fn hiding_a_model_writes_the_list_and_a_failure_restores_it(cx: &mut TestAppContext) {
        let fixture = fixture("anthropic");
        let (window, workspace, page) = opened(&fixture, cx);
        act(window, &page, cx, |page, window, cx| {
            page.toggle_hidden("anthropic", "sonnet", window, cx);
        });
        let hidden = vec![("anthropic".to_owned(), "sonnet".to_owned())];
        assert_eq!(loaded(&page, cx).confirmed_hidden, hidden);
        assert_eq!(
            fixture.document.lock().expect("document")["hiddenModels"],
            json!([{"providerID": "anthropic", "modelID": "sonnet"}])
        );
        cx.read(|cx| {
            let settings = workspace.read(cx).send_catalog.settings.as_ref();
            assert_eq!(
                settings.map(|settings| settings.hidden_models.clone()),
                Some(hidden.clone()),
                "the composer's pickers hide it too"
            );
        });
        fixture.fail_writes.store(true, Ordering::Release);
        act(window, &page, cx, |page, window, cx| {
            page.set_all_hidden("anthropic", true, window, cx);
        });
        assert_eq!(
            loaded(&page, cx).hidden,
            hidden,
            "the rows show what the server holds"
        );
        cx.read(|cx| assert!(page.read(cx).has_failed_writes()));
        fixture.fail_writes.store(false, Ordering::Release);
        act(window, &page, cx, |page, window, cx| page.retry(window, cx));
        assert_eq!(loaded(&page, cx).confirmed_hidden.len(), 2);
        cx.read(|cx| assert!(!page.read(cx).has_failed_writes()));
    }

    #[gpui_kit::test]
    fn a_change_invalidates_the_composer_catalog_and_the_usage_samples(cx: &mut TestAppContext) {
        let fixture = fixture("anthropic");
        let (window, workspace, page) = opened(&fixture, cx);
        workspace.update(cx, |workspace, cx| {
            workspace
                .send_catalog
                .insert_test_directory("/repo".into(), Default::default());
            workspace
                .usage_quota
                .update(cx, |quota, cx| quota.ensure_all(cx));
        });
        settle(window, cx);
        let (catalog, quota) = (
            count(&fixture, "GET", "/api/model/default"),
            count(&fixture, "GET", "/api/quota/"),
        );
        assert!(quota > 0, "the usage samples were read once");
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Candidate("openrouter".into()), window, cx);
        });
        type_key(window, &page, KEY, cx);
        act(window, &page, cx, |page, window, cx| {
            page.connect_key(window, cx);
        });
        settle(window, cx);
        assert!(
            count(&fixture, "GET", "/api/model/default") > catalog,
            "the composer's model catalog was read again"
        );
        assert!(
            count(&fixture, "GET", "/api/quota/") > quota,
            "the usage samples were asked again"
        );
    }

    #[gpui_kit::test]
    fn a_hidden_page_reads_nothing_and_drops_its_fields(cx: &mut TestAppContext) {
        let first = fixture("old-server-provider");
        let second = fixture("anthropic");
        let (window, workspace, page) = opened(&first, cx);
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Candidate("openrouter".into()), window, cx);
        });
        type_key(window, &page, KEY, cx);
        act(window, &page, cx, |page, _, cx| page.set_shown(false, cx));
        cx.read(|cx| {
            assert!(
                page.read(cx).inputs.is_none(),
                "a hidden page holds no key text"
            );
        });
        let reads = cx.read(|cx| page.read(cx).read_revision);
        retarget(&workspace, &second.server, cx);
        settle(window, cx);
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.read_revision, reads, "a hidden page starts no read");
            assert_eq!(page.load, Load::Loading);
        });
        act(window, &page, cx, |page, _, cx| page.set_shown(true, cx));
        assert_eq!(loaded(&page, cx).providers[0].id, "anthropic");
    }

    #[gpui_kit::test]
    fn a_runtime_switch_discards_the_retired_servers_work(cx: &mut TestAppContext) {
        let gate = ResponseGate::new();
        let first = gated_fixture("old-server-provider", Some(Arc::clone(&gate)));
        let second = fixture("anthropic");
        let (window, workspace, page) = opened(&first, cx);
        act(window, &page, cx, |page, window, cx| {
            page.open(View::Candidate("openrouter".into()), window, cx);
        });
        type_key(window, &page, KEY, cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| page.connect_key(window, cx));
        })
        .expect("test window");
        gate.entered(cx);
        // The key write is still unanswered when the server changes.
        retarget(&workspace, &second.server, cx);
        settle(window, cx);
        gate.release();
        settle(window, cx);
        assert_eq!(loaded(&page, cx).providers[0].id, "anthropic");
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.view, View::List, "the old server's view is let go");
            assert!(page.connecting_keys.is_empty());
            assert!(!page.has_failed_writes());
            assert_eq!(
                page.notice, None,
                "the retired server's answer is not reported for the new one"
            );
        });
        assert_eq!(
            key_text(&page, cx),
            "",
            "the key typed for the old server is gone"
        );
        assert!(second.server.writes().is_empty());
    }
}
