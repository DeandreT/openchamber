//! What the composer sends with and the Work status card counts: the
//! catalogs of the active config directory, OpenChamber's defaults and the
//! selection they resolve to.
//!
//! Mirrors the catalog loads of `packages/ui/src/stores/useConfigStore.ts`:
//! the config directory is the selected session's directory, or on the
//! new-session view the last directory when it is a known project or
//! worktree, else the active project, else the first one. Each directory's
//! catalogs load once and reload when OpenCode announces a rebuild; a reread
//! that fails keeps what the directory held. OpenCode starts a location
//! lazily and plugins register agents while it already serves, so the first
//! directory is read again once startup has settled, as the web client does.

use std::collections::HashMap;
use std::time::Duration;

use gpui_kit::{Context, Task};
use openchamber_native_core::path;
use openchamber_native_core::send_config::{
    self, AgentColor, AgentEntry, AutoRouting, CatalogProvider, ModelChoice, Selection,
    SelectionDefaults, SessionRecordChoice,
};
use openchamber_native_transport::catalog::{ModelCatalog, OpenCodeDefaults};
use openchamber_native_transport::client::ServerSettings;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::CatalogKind;

use crate::theme::Palette;
use crate::workspace::Workspace;

/// Reads per catalog before a load gives up, and the growing pause between
/// them (`loadAgents` and `loadProviders`).
const READ_ATTEMPTS: u32 = 3;
const ATTEMPT_PAUSE: Duration = Duration::from_millis(200);
/// When the first directory is read again (`startupAgentRecheck`).
const STARTUP_RECHECK: Duration = Duration::from_secs(8);
/// The model `openchamber/auto` routes per prompt.
const AUTO_PROVIDER: &str = "openchamber";
const AUTO_MODEL: &str = "auto";

/// Where one catalog of a directory stands.
#[derive(Debug, Default)]
pub(crate) enum Load<T> {
    #[default]
    Loading,
    Ready(T),
    /// The read failed and nothing was held before.
    Failed,
}

impl<T> Load<T> {
    pub(crate) fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            Self::Loading | Self::Failed => None,
        }
    }

    /// Takes a read's result; a failed reread keeps what was held.
    fn settle(&mut self, outcome: Option<T>) {
        match outcome {
            Some(value) => *self = Self::Ready(value),
            None if matches!(self, Self::Ready(_)) => {}
            None => *self = Self::Failed,
        }
    }
}

/// The catalogs of one config directory.
#[derive(Default)]
pub(crate) struct DirectoryCatalog {
    pub agents: Load<Vec<AgentEntry>>,
    pub providers: Load<Vec<CatalogProvider>>,
    pub opencode: Load<OpenCodeDefaults>,
    pub skills: Load<usize>,
    pub mcp_connected: Load<usize>,
    reads: Vec<Task<()>>,
}

/// OpenChamber's settings and each visited directory's catalogs.
#[derive(Default)]
pub(crate) struct SendCatalog {
    /// Absent until the settings document was read.
    pub settings: Option<ServerSettings>,
    directories: HashMap<String, DirectoryCatalog>,
    recheck: Option<Task<()>>,
}

impl SendCatalog {
    pub(crate) fn directory(&self, directory: &str) -> Option<&DirectoryCatalog> {
        self.directories.get(directory)
    }
}

/// Which reads a load makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reads {
    agents: bool,
    models: bool,
    config: bool,
    skills: bool,
    mcp: bool,
}

impl Reads {
    const ALL: Self = Self {
        agents: true,
        models: true,
        config: true,
        skills: true,
        mcp: true,
    };

    /// What a rebuilt catalog changes, as `catalogRefresh.ts` rereads it.
    fn for_kind(kind: CatalogKind) -> Self {
        let none = Self {
            agents: false,
            models: false,
            config: false,
            skills: false,
            mcp: false,
        };
        match kind {
            CatalogKind::Config => Self::ALL,
            CatalogKind::Agent => Self {
                agents: true,
                ..none
            },
            CatalogKind::Skill => Self {
                skills: true,
                ..none
            },
            CatalogKind::Provider | CatalogKind::Model | CatalogKind::Credential => Self {
                models: true,
                ..none
            },
            CatalogKind::McpStatus => Self { mcp: true, ..none },
        }
    }
}

/// The results of one load; `None` where a read was not made.
struct Outcome {
    agents: Option<Result<Vec<AgentEntry>, TransportError>>,
    models: Option<Result<ModelCatalog, TransportError>>,
    config: Option<Result<OpenCodeDefaults, TransportError>>,
    skills: Option<Result<usize, TransportError>>,
    mcp: Option<Result<usize, TransportError>>,
}

/// What the composer's model control shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModelControl {
    /// The catalogs are still loading, or could not be read.
    Pending { unavailable: bool },
    /// Nothing is selected.
    Unselected,
    /// `openchamber/auto`.
    Auto,
    Model {
        provider_id: String,
        model_id: String,
        label: String,
        variant: VariantControl,
    },
}

/// What the composer's reasoning effort control shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VariantControl {
    /// The model offers no efforts.
    Hidden,
    /// The model's default effort.
    Default,
    Selected(String),
}

/// What the composer's agent control shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentControl {
    Pending {
        unavailable: bool,
    },
    /// No agent is selected: the label of the agent a send would use, muted.
    Default {
        label: Option<String>,
    },
    Agent {
        label: String,
        color: AgentColor,
    },
}

impl Workspace {
    /// The directory whose catalogs the composer uses.
    pub(crate) fn config_directory(&self) -> Option<String> {
        if let Some(session) = self.selected_session() {
            return session.directory.as_deref().and_then(path::normalize);
        }
        let settings = self.send_catalog.settings.as_ref()?;
        let known = |directory: &str| {
            self.topology
                .projects
                .iter()
                .any(|project| project.path == directory)
                || self.topology.worktrees.iter().any(|entry| {
                    entry
                        .worktrees
                        .iter()
                        .any(|worktree| worktree.path == directory)
                })
        };
        settings
            .last_directory
            .as_deref()
            .and_then(path::normalize)
            .filter(|directory| known(directory))
            .or_else(|| settings.active_project_path().map(str::to_owned))
            .or_else(|| {
                settings
                    .projects
                    .first()
                    .map(|project| project.path.clone())
            })
    }

    /// The project a directory belongs to: itself when registered, else the
    /// project of the worktree it is.
    fn owning_project(&self, directory: &str) -> Option<&str> {
        if let Some(project) = self
            .topology
            .projects
            .iter()
            .find(|project| project.path == directory)
        {
            return Some(&project.path);
        }
        self.topology
            .worktrees
            .iter()
            .find(|entry| {
                entry
                    .worktrees
                    .iter()
                    .any(|worktree| worktree.path == directory)
            })
            .map(|entry| entry.project_path.as_str())
    }

    /// Starts loading the config directory's catalogs if nothing holds them.
    /// Called while rendering, so every way the directory changes is covered.
    pub(crate) fn ensure_send_catalog(&mut self, cx: &mut Context<Self>) {
        if self.send_catalog.settings.is_none() {
            return;
        }
        let Some(directory) = self.config_directory() else {
            return;
        };
        if self.send_catalog.directories.contains_key(&directory) {
            return;
        }
        let first = self.send_catalog.directories.is_empty();
        self.send_catalog
            .directories
            .insert(directory.clone(), DirectoryCatalog::default());
        self.load_catalog(&directory, Reads::ALL, cx);
        if first {
            self.send_catalog.recheck = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(STARTUP_RECHECK).await;
                let _ = this.update(cx, |this, cx| {
                    this.load_catalog(&directory, Reads::for_kind(CatalogKind::Agent), cx);
                });
            }));
        }
    }

    /// Rereads what a rebuilt catalog changed, in its directory or in every
    /// directory held when the event names none.
    pub(crate) fn refresh_send_catalog(
        &mut self,
        kind: CatalogKind,
        directory: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let directories: Vec<String> = match directory.and_then(path::normalize) {
            Some(directory) if self.send_catalog.directories.contains_key(&directory) => {
                vec![directory]
            }
            Some(_) => Vec::new(),
            None => self.send_catalog.directories.keys().cloned().collect(),
        };
        for directory in directories {
            self.load_catalog(&directory, Reads::for_kind(kind), cx);
        }
    }

    /// Forgets every directory's catalogs, as a new runtime must.
    pub(crate) fn reset_send_catalog(&mut self) {
        self.send_catalog = SendCatalog::default();
    }

    fn load_catalog(&mut self, directory: &str, reads: Reads, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let skills_directory = self
            .owning_project(directory)
            .unwrap_or(directory)
            .to_owned();
        let read_directory = directory.to_owned();
        let task = connection.network.run(async move {
            let directory = read_directory.as_str();
            let (agents, models, config, skills, mcp) = tokio::join!(
                read(reads.agents, || client.agents(directory)),
                read(reads.models, || client.model_catalog(directory)),
                read(reads.config, || client.opencode_defaults(directory)),
                read(reads.skills, || client.skill_count(&skills_directory)),
                read(reads.mcp, || client.connected_mcp_servers(directory)),
            );
            Outcome {
                agents,
                models,
                config,
                skills,
                mcp,
            }
        });
        let directory = directory.to_owned();
        let key = directory.clone();
        let read = cx.spawn(async move |this, cx| {
            let Some(outcome) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.apply_catalog(&directory, outcome);
                cx.notify();
            });
        });
        if let Some(entry) = self.send_catalog.directories.get_mut(&key) {
            entry.reads.retain(|task| !task.is_ready());
            entry.reads.push(read);
        }
    }

    fn apply_catalog(&mut self, directory: &str, outcome: Outcome) {
        let Some(entry) = self.send_catalog.directories.get_mut(directory) else {
            return;
        };
        fn settle<T>(load: &mut Load<T>, result: Option<Result<T, TransportError>>, what: &str) {
            match result {
                None => {}
                Some(Ok(value)) => load.settle(Some(value)),
                Some(Err(error)) => {
                    tracing::warn!(%error, "the {what} could not be read");
                    load.settle(None);
                }
            }
        }
        settle(&mut entry.agents, outcome.agents, "agents");
        let providers = outcome.models.map(|result| {
            result.map(|catalog| send_config::group_catalog(&catalog.providers, &catalog.models))
        });
        settle(&mut entry.providers, providers, "models");
        settle(&mut entry.opencode, outcome.config, "OpenCode config");
        settle(&mut entry.skills, outcome.skills, "skills");
        settle(&mut entry.mcp_connected, outcome.mcp, "MCP servers");
    }

    /// The defaults the selection resolves from, for `directory`.
    fn selection_defaults(
        &self,
        directory: &str,
        opencode: &OpenCodeDefaults,
    ) -> SelectionDefaults {
        let Some(settings) = self.send_catalog.settings.as_ref() else {
            return SelectionDefaults::default();
        };
        let project = self.owning_project(directory).and_then(|project| {
            settings
                .project_defaults
                .iter()
                .find(|defaults| defaults.path == project)
        });
        SelectionDefaults {
            project_default_agent: project.and_then(|project| project.agent.clone()),
            project_default_model: project.and_then(|project| project.model.clone()),
            project_default_variant: project.and_then(|project| project.variant.clone()),
            settings_default_agent: settings.default_agent.clone(),
            settings_default_model: settings.default_model.clone(),
            settings_default_variant: settings.default_variant.clone(),
            opencode_default_agent: opencode.default_agent.clone(),
            opencode_default_model: opencode.model.clone(),
            last_selected_model: settings.last_selected_model.clone(),
            hidden_models: settings.hidden_models.clone(),
            allow_fallback: true,
            // Routing readiness is not read yet: an available feature keeps
            // an Auto choice, as unknown readiness does in the web client.
            auto_routing: match settings.routing_feature_available {
                Some(true) => AutoRouting::Unknown,
                _ => AutoRouting::Unavailable,
            },
        }
    }

    /// What a send would use, once the config directory's agents, models
    /// and config are known: the directory's default selection, refined by a
    /// selected session's record.
    fn send_selection(&self) -> Option<(String, Selection)> {
        let directory = self.config_directory()?;
        let entry = self.send_catalog.directory(&directory)?;
        let agents = entry.agents.ready()?;
        let providers = entry.providers.ready()?;
        let fallback = OpenCodeDefaults::default();
        let opencode = match &entry.opencode {
            Load::Ready(opencode) => opencode,
            Load::Failed => &fallback,
            Load::Loading => return None,
        };
        let defaults = self.selection_defaults(&directory, opencode);
        let selection = send_config::resolve_default_selection(agents, providers, &defaults);
        let selection = match self.selected_session() {
            Some(session) => send_config::resolve_session_selection(
                &selection,
                SessionRecordChoice {
                    model: session.model.as_ref(),
                    agent: session.agent.as_deref(),
                },
                agents,
                providers,
                &defaults,
            ),
            None => selection,
        };
        Some((directory, selection))
    }

    /// Whether the catalogs could not be read and are not coming.
    fn catalog_unavailable(&self) -> bool {
        let Some(directory) = self.config_directory() else {
            return false;
        };
        self.send_catalog
            .directory(&directory)
            .is_some_and(|entry| {
                matches!(entry.agents, Load::Failed) || matches!(entry.providers, Load::Failed)
            })
    }

    pub(crate) fn model_control(&self) -> ModelControl {
        let unavailable = self.catalog_unavailable();
        let Some((directory, selection)) = self.send_selection() else {
            return ModelControl::Pending { unavailable };
        };
        let Some(choice) = selection.model else {
            return ModelControl::Unselected;
        };
        if is_auto(&choice) {
            return ModelControl::Auto;
        }
        let providers = self
            .send_catalog
            .directory(&directory)
            .and_then(|entry| entry.providers.ready())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let offers_variants = send_config::find_model(providers, &choice)
            .is_some_and(|model| !model.variants.is_empty());
        let variant = match (offers_variants, choice.variant.as_ref()) {
            (false, _) => VariantControl::Hidden,
            (true, None) => VariantControl::Default,
            (true, Some(variant)) => VariantControl::Selected(variant.clone()),
        };
        ModelControl::Model {
            label: send_config::model_label(providers, &choice),
            provider_id: choice.provider_id,
            model_id: choice.model_id,
            variant,
        }
    }

    pub(crate) fn agent_control(&self, palette: &Palette) -> AgentControl {
        let unavailable = self.catalog_unavailable();
        let Some((directory, selection)) = self.send_selection() else {
            return AgentControl::Pending { unavailable };
        };
        let agents = self
            .send_catalog
            .directory(&directory)
            .and_then(|entry| entry.agents.ready())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let Some(name) = selection.agent_name else {
            // `getAgentDisplayName` without a choice: Build, else the first
            // primary agent.
            let primary = || {
                agents
                    .iter()
                    .filter(|agent| !agent.hidden && is_primary(agent))
            };
            let shown = primary()
                .find(|agent| agent.name == "build")
                .or_else(|| primary().next());
            return AgentControl::Default {
                label: shown.map(send_config::agent_label),
            };
        };
        let label = agents
            .iter()
            .find(|agent| agent.name == name)
            .map(send_config::agent_label)
            .unwrap_or_else(|| send_config::agent_name_label(&name));
        let color = send_config::agent_color_with_palette(
            Some(&name),
            agents,
            &palette.agent_palette,
            palette.agent_syntax.len(),
        );
        AgentControl::Agent { label, color }
    }

    /// The Work status card's ambient counts: skills, then connected MCP
    /// servers, or `None` while neither is known.
    pub(crate) fn context_counts(&self) -> Option<(usize, usize)> {
        let directory = self.config_directory()?;
        let entry = self.send_catalog.directory(&directory)?;
        let skills = entry.skills.ready().copied();
        let mcp = entry.mcp_connected.ready().copied();
        if skills.is_none() && mcp.is_none() {
            return None;
        }
        Some((skills.unwrap_or(0), mcp.unwrap_or(0)))
    }
}

/// Makes one read, retrying a failure with a growing pause.
async fn read<T, F, Fut>(wanted: bool, mut attempt: F) -> Option<Result<T, TransportError>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    if !wanted {
        return None;
    }
    let mut tries = 0;
    loop {
        tries += 1;
        match attempt().await {
            Ok(value) => return Some(Ok(value)),
            Err(error @ (TransportError::AuthenticationRequired | TransportError::Cancelled)) => {
                return Some(Err(error));
            }
            Err(error) if tries >= READ_ATTEMPTS => return Some(Err(error)),
            Err(_) => tokio::time::sleep(ATTEMPT_PAUSE * tries).await,
        }
    }
}

fn is_auto(choice: &ModelChoice) -> bool {
    choice.provider_id == AUTO_PROVIDER && choice.model_id == AUTO_MODEL
}

/// `isPrimaryMode`: an absent mode is primary.
fn is_primary(agent: &AgentEntry) -> bool {
    !matches!(agent.mode, Some(send_config::AgentMode::Subagent))
}
