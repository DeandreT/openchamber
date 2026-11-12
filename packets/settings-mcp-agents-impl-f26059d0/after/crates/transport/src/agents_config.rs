//! Settings → Agents (`useAgentsStore.ts`): the agents of one directory with
//! where each is defined, an agent's own stored entry and permission rules,
//! the agents a config file switches off, and the writes through
//! `/api/config/agents/:name`.
//!
//! The editor works from an agent's own stored entry
//! (`/api/config/agents/:name/config`), never from the resolved catalog
//! entry: that one already merges built-in defaults and global config, and
//! writing it back would bake them into the agent's file.

use futures_util::future;
use openchamber_native_core::send_config::{AgentEntry, AgentMode};
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, encode_directory};
use crate::error::TransportError;
use crate::files::read_answer;

/// Where an agent's file lives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentScope {
    /// The user's global configuration.
    #[default]
    User,
    Project,
}

/// What a permission rule decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionEffect {
    Allow,
    Ask,
    Deny,
}

/// One entry of an agent's ordered `permissions` list. The last matching
/// rule wins, so the order is part of the policy.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct PermissionRule {
    pub action: String,
    pub resource: String,
    pub effect: PermissionEffect,
}

/// The config spelling of a mode.
pub fn mode_as_wire(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Primary => "primary",
        AgentMode::Subagent => "subagent",
        AgentMode::All => "all",
    }
}

fn mode_from_wire(mode: &str) -> Option<AgentMode> {
    match mode {
        "primary" => Some(AgentMode::Primary),
        "subagent" => Some(AgentMode::Subagent),
        "all" => Some(AgentMode::All),
        _ => None,
    }
}

/// An agent of the directory with the facts its config file adds to the
/// catalog entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsAgent {
    pub entry: AgentEntry,
    pub scope: Option<AgentScope>,
    /// The subfolder of `agents/` the file sits in.
    pub group: Option<String>,
    /// The file the next save rewrites.
    pub path: Option<String>,
    /// The file still uses OpenCode 1 spellings.
    pub legacy: bool,
    /// Whether no file or config entry of the user defines the agent. `None`
    /// when the lookup failed, and then the flag is unknown.
    pub built_in: Option<bool>,
}

impl SettingsAgent {
    /// Whether Settings lists the agent (`isAgentManageable`). A hidden agent
    /// counts as custom only when its lookup confirmed it: OpenCode's
    /// internal agents stay out.
    pub fn is_manageable(&self) -> bool {
        !self.entry.hidden || self.built_in == Some(false)
    }

    /// Whether a chat can run as the agent (`isPrimaryMode`, not hidden).
    pub fn is_selectable_primary(&self) -> bool {
        !self.entry.hidden && self.entry.mode != Some(AgentMode::Subagent)
    }
}

#[derive(Default, Deserialize)]
struct SourceWire {
    #[serde(default)]
    exists: bool,
    scope: Option<AgentScope>,
    path: Option<String>,
    #[serde(default)]
    legacy: bool,
}

#[derive(Default, Deserialize)]
struct SourcesWire {
    #[serde(default)]
    md: SourceWire,
    #[serde(default)]
    json: SourceWire,
}

/// `GET /api/config/agents/:name`.
#[derive(Deserialize)]
struct LookupWire {
    scope: Option<AgentScope>,
    #[serde(default)]
    sources: SourcesWire,
    #[serde(default, rename = "isBuiltIn")]
    is_built_in: bool,
}

/// The subfolder an agent file sits in (`parseAgentGroup`):
/// `…/agents/business/ceo.md` is in `business`, `…/agents/ceo.md` in none.
fn agent_group(path: Option<&str>) -> Option<String> {
    let path = path?.replace('\\', "/");
    let (_, relative) = path.rsplit_once("/agents/")?;
    let (group, _) = relative.split_once('/')?;
    Some(group.to_owned())
}

/// An agent's own stored entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentEntity {
    pub description: Option<String>,
    /// The joined `provider/model#variant` string.
    pub model: Option<String>,
    pub system: Option<String>,
    pub mode: Option<AgentMode>,
    pub hidden: Option<bool>,
    pub color: Option<String>,
    pub steps: Option<u64>,
    /// The `request` block as stored. A write replaces it whole, so the keys
    /// the editor does not own are carried from here.
    pub request: Option<Value>,
    pub permissions: Vec<PermissionRule>,
}

#[derive(Default, Deserialize)]
struct EntityWire {
    description: Option<String>,
    model: Option<String>,
    system: Option<String>,
    mode: Option<String>,
    hidden: Option<bool>,
    color: Option<String>,
    steps: Option<u64>,
    request: Option<Value>,
    #[serde(default)]
    permissions: Option<Vec<PermissionRule>>,
}

/// Which kind of source defines an agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSource {
    Markdown,
    Json,
    /// No file of the user defines it: OpenCode or a plugin provides it.
    None,
}

/// `GET /api/config/agents/:name/config`.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentEnvelope {
    pub source: AgentSource,
    pub scope: Option<AgentScope>,
    pub path: Option<String>,
    pub legacy: bool,
    pub config: AgentEntity,
}

#[derive(Deserialize)]
struct EnvelopeWire {
    source: Option<String>,
    scope: Option<AgentScope>,
    path: Option<String>,
    #[serde(default)]
    legacy: bool,
    #[serde(default)]
    config: Option<EntityWire>,
}

/// `GET /api/config/agents/:name/permissions`: the global config's rules and
/// the agent's own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentPermissions {
    pub global: Vec<PermissionRule>,
    pub agent: Vec<PermissionRule>,
}

#[derive(Deserialize)]
struct PermissionsWire {
    #[serde(default)]
    global: Option<Vec<PermissionRule>>,
    #[serde(default)]
    agent: Option<Vec<PermissionRule>>,
}

/// An agent a config file switches off.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DisabledAgent {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
struct DisabledWire {
    agents: Vec<DisabledAgent>,
}

/// What a create or an update writes. An absent field is left as stored;
/// `Some(None)` removes the key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentChanges {
    pub mode: Option<AgentMode>,
    pub description: Option<String>,
    pub model: Option<Option<String>>,
    pub steps: Option<Option<u64>>,
    pub system: Option<Option<String>>,
    pub color: Option<Option<String>>,
    pub hidden: Option<bool>,
    /// `false` removes the key, which turns a disabled agent back on.
    pub disabled: Option<bool>,
    /// The whole `request` block.
    pub request: Option<Option<Value>>,
    pub permissions: Option<Vec<PermissionRule>>,
}

impl AgentChanges {
    /// The body of an update (`updateAgent`).
    fn body(&self) -> Map<String, Value> {
        let mut body = Map::new();
        if let Some(mode) = self.mode {
            body.insert("mode".to_owned(), json!(mode_as_wire(mode)));
        }
        if let Some(description) = &self.description {
            body.insert("description".to_owned(), json!(description));
        }
        if let Some(model) = &self.model {
            body.insert("model".to_owned(), json!(model));
        }
        if let Some(steps) = self.steps {
            body.insert("steps".to_owned(), json!(steps));
        }
        if let Some(system) = &self.system {
            body.insert("system".to_owned(), json!(system));
        }
        if let Some(color) = &self.color {
            body.insert("color".to_owned(), json!(color));
        }
        if let Some(hidden) = self.hidden {
            body.insert("hidden".to_owned(), json!(hidden));
        }
        if let Some(disabled) = self.disabled {
            body.insert(
                "disabled".to_owned(),
                if disabled { json!(true) } else { Value::Null },
            );
        }
        if let Some(request) = &self.request {
            body.insert("request".to_owned(), json!(request));
        }
        if let Some(permissions) = &self.permissions {
            body.insert("permissions".to_owned(), json!(permissions));
        }
        body
    }

    /// The body of a create (`createAgent`): only what says something, a
    /// mode in any case, and the scope.
    fn create_body(&self, scope: AgentScope) -> Map<String, Value> {
        let mut body = self.body();
        body.remove("disabled");
        body.retain(|_, value| {
            !value.is_null()
                && value.as_str() != Some("")
                && value.as_array().is_none_or(|rules| !rules.is_empty())
        });
        body.entry("mode").or_insert_with(|| json!("subagent"));
        body.insert("scope".to_owned(), json!(scope));
        body
    }
}

/// Why an agent write did not land.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentError {
    #[error("the server refused the write (HTTP {status})")]
    Refused {
        status: u16,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl AgentError {
    /// The server's reason for a refusal.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Refused { message, .. } => message.as_deref(),
            Self::Transport(_) => None,
        }
    }
}

impl ServerClient {
    fn agent_config_url(&self, name: &str, resource: Option<&str>, directory: &str) -> Url {
        let mut url = self.endpoint().url("api/config/agents/");
        {
            let mut segments = url
                .path_segments_mut()
                .expect("HTTP URLs have path segments");
            segments.pop_if_empty().push(name);
            if let Some(resource) = resource {
                segments.push(resource);
            }
        }
        url.query_pairs_mut().append_pair("directory", directory);
        url
    }

    /// The agents of `directory`, each with where it is defined. A failed
    /// list is an error. A failed lookup of one agent keeps it as the
    /// catalog listed it, with its built-in flag unknown, and leaves the
    /// others complete.
    pub async fn settings_agents(
        &self,
        directory: &str,
    ) -> Result<Vec<SettingsAgent>, TransportError> {
        let listed = self.agents(directory).await?;
        let lookups = listed.into_iter().map(|entry| async move {
            let url = self.agent_config_url(&entry.name, None, directory);
            match self.get_json::<LookupWire>(url, Some(directory)).await {
                Ok(lookup) => {
                    let sources = lookup.sources;
                    let active = if sources.md.exists {
                        Some(&sources.md)
                    } else if sources.json.exists {
                        Some(&sources.json)
                    } else {
                        None
                    };
                    SettingsAgent {
                        scope: lookup
                            .scope
                            .or(active.and_then(|source| source.scope))
                            .or(sources.md.scope)
                            .or(sources.json.scope),
                        group: agent_group(sources.md.path.as_deref()),
                        path: active.and_then(|source| source.path.clone()),
                        legacy: active.is_some_and(|source| source.legacy),
                        built_in: Some(lookup.is_built_in),
                        entry,
                    }
                }
                Err(_) => SettingsAgent {
                    entry,
                    scope: None,
                    group: None,
                    path: None,
                    legacy: false,
                    built_in: None,
                },
            }
        });
        Ok(future::join_all(lookups).await)
    }

    /// Agent `name`'s own stored entry.
    pub async fn agent_entity(
        &self,
        name: &str,
        directory: &str,
    ) -> Result<AgentEnvelope, TransportError> {
        let envelope: EnvelopeWire = self
            .get_json(
                self.agent_config_url(name, Some("config"), directory),
                Some(directory),
            )
            .await?;
        let config = envelope.config.unwrap_or_default();
        Ok(AgentEnvelope {
            source: match envelope.source.as_deref() {
                Some("md") => AgentSource::Markdown,
                Some("json") => AgentSource::Json,
                _ => AgentSource::None,
            },
            scope: envelope.scope,
            path: envelope.path,
            legacy: envelope.legacy,
            config: AgentEntity {
                description: config.description,
                model: config.model,
                system: config.system,
                mode: config.mode.as_deref().and_then(mode_from_wire),
                hidden: config.hidden,
                color: config.color,
                steps: config.steps,
                request: config.request.filter(|request| !request.is_null()),
                permissions: config.permissions.unwrap_or_default(),
            },
        })
    }

    /// The permission rules that decide for agent `name`.
    pub async fn agent_permissions(
        &self,
        name: &str,
        directory: &str,
    ) -> Result<AgentPermissions, TransportError> {
        let permissions: PermissionsWire = self
            .get_json(
                self.agent_config_url(name, Some("permissions"), directory),
                Some(directory),
            )
            .await?;
        Ok(AgentPermissions {
            global: permissions.global.unwrap_or_default(),
            agent: permissions.agent.unwrap_or_default(),
        })
    }

    /// The agents `directory`'s config files switch off.
    pub async fn disabled_agents(
        &self,
        directory: &str,
    ) -> Result<Vec<DisabledAgent>, TransportError> {
        let mut url = self.endpoint().url("api/config/disabled-agents");
        url.query_pairs_mut().append_pair("directory", directory);
        let disabled: DisabledWire = self.get_json(url, Some(directory)).await?;
        Ok(disabled.agents)
    }

    async fn agent_write(
        &self,
        method: Method,
        name: &str,
        directory: &str,
        body: Map<String, Value>,
    ) -> Result<(), AgentError> {
        let response = self
            .authorized(method, self.agent_config_url(name, None, directory))
            .header("x-opencode-directory", encode_directory(directory))
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        match read_answer(response).await? {
            Ok(_) => Ok(()),
            Err(refusal) => Err(AgentError::Refused {
                status: refusal.status.as_u16(),
                message: refusal.message,
            }),
        }
    }

    /// Creates agent `name` in `scope`.
    pub async fn create_agent(
        &self,
        name: &str,
        changes: &AgentChanges,
        scope: AgentScope,
        directory: &str,
    ) -> Result<(), AgentError> {
        self.agent_write(Method::POST, name, directory, changes.create_body(scope))
            .await
    }

    /// Writes `changes` into agent `name`'s file.
    pub async fn update_agent(
        &self,
        name: &str,
        changes: &AgentChanges,
        directory: &str,
    ) -> Result<(), AgentError> {
        self.agent_write(Method::PATCH, name, directory, changes.body())
            .await
    }

    /// Deletes agent `name`; `scope` names the file when it is known.
    pub async fn delete_agent(
        &self,
        name: &str,
        scope: Option<AgentScope>,
        directory: &str,
    ) -> Result<(), AgentError> {
        let mut body = Map::new();
        if let Some(scope) = scope {
            body.insert("scope".to_owned(), json!(scope));
        }
        self.agent_write(Method::DELETE, name, directory, body)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_follow_the_store_writers() {
        let changes = AgentChanges {
            mode: Some(AgentMode::Primary),
            description: Some(String::new()),
            model: Some(None),
            steps: Some(None),
            system: Some(Some("Be brief".into())),
            color: Some(None),
            hidden: None,
            disabled: Some(false),
            request: Some(Some(json!({"body": {"temperature": 0.2}}))),
            permissions: Some(Vec::new()),
        };
        assert_eq!(
            Value::Object(changes.body()),
            json!({
                "mode": "primary",
                "description": "",
                "model": null,
                "steps": null,
                "system": "Be brief",
                "color": null,
                "disabled": null,
                "request": {"body": {"temperature": 0.2}},
                "permissions": [],
            })
        );
        assert_eq!(
            Value::Object(changes.create_body(AgentScope::Project)),
            json!({
                "mode": "primary",
                "system": "Be brief",
                "request": {"body": {"temperature": 0.2}},
                "scope": "project",
            }),
            "a create writes nothing empty"
        );
        assert_eq!(
            Value::Object(AgentChanges::default().create_body(AgentScope::User)),
            json!({"mode": "subagent", "scope": "user"})
        );
        assert_eq!(
            AgentChanges {
                disabled: Some(true),
                ..AgentChanges::default()
            }
            .body()["disabled"],
            true
        );
        assert_eq!(
            agent_group(Some("C:\\cfg\\agents\\business\\ceo.md")).as_deref(),
            Some("business")
        );
        assert_eq!(agent_group(Some("/cfg/agents/ceo.md")), None);
        assert_eq!(agent_group(None), None);
    }

    fn route(request: &crate::test_server::Recorded) -> (u16, Value) {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/api/agent") => (
                200,
                json!({"location": {}, "data": [
                    {"id": "build", "mode": "primary"},
                    {"id": "reviewer", "mode": "subagent", "description": "Reviews"},
                    {"id": "title", "hidden": true},
                    {"id": "broken"},
                ]}),
            ),
            ("GET", "/api/config/agents/build") => (
                200,
                json!({"name": "build", "scope": null, "isBuiltIn": true,
                       "sources": {"md": {"exists": false}, "json": {"exists": false}}}),
            ),
            ("GET", "/api/config/agents/reviewer") => (
                200,
                json!({"name": "reviewer", "scope": "project", "isBuiltIn": false,
                       "sources": {"md": {"exists": true, "scope": "project", "legacy": true,
                                          "path": "/repo/.opencode/agents/team/reviewer.md"},
                                   "json": {"exists": false}}}),
            ),
            ("GET", "/api/config/agents/title") => (
                200,
                json!({"name": "title", "scope": null, "isBuiltIn": true, "sources": {}}),
            ),
            ("GET", "/api/config/agents/reviewer/config") => (
                200,
                json!({"source": "md", "scope": "project", "legacy": true,
                       "path": "/repo/.opencode/agents/team/reviewer.md",
                       "config": {"description": "Reviews", "model": "anthropic/claude#high",
                                  "mode": "subagent", "steps": 12,
                                  "request": {"headers": {"x": "y"}, "body": {"temperature": 0.3}},
                                  "permissions": [
                                      {"action": "shell", "resource": "*", "effect": "ask"}]}}),
            ),
            ("GET", "/api/config/agents/reviewer/permissions") => (
                200,
                json!({"global": [{"action": "*", "resource": "*", "effect": "ask"}],
                       "agent": [{"action": "shell", "resource": "*", "effect": "deny"}],
                       "effective": [], "source": "md", "path": null}),
            ),
            ("GET", "/api/config/disabled-agents") => (
                200,
                json!({"agents": [{"name": "old", "scope": "user", "path": null}]}),
            ),
            ("GET", _) => (500, json!({"error": "unreadable"})),
            ("DELETE", _) => (500, json!({"error": "Agent is built-in"})),
            _ => (200, json!({"success": true})),
        }
    }

    #[tokio::test]
    async fn agents_carry_their_definition_and_one_failed_lookup_stays_unknown() {
        let (client, recorded) = crate::test_server::serve(Box::new(route)).await;
        let agents = client.settings_agents("/repo").await.expect("agents");
        assert_eq!(agents.len(), 4);
        assert_eq!(agents[0].built_in, Some(true));
        assert!(agents[0].is_manageable() && agents[0].is_selectable_primary());
        assert_eq!(agents[1].scope, Some(AgentScope::Project));
        assert_eq!(agents[1].group.as_deref(), Some("team"));
        assert!(agents[1].legacy && !agents[1].is_selectable_primary());
        assert!(
            !agents[2].is_manageable(),
            "OpenCode's internal agents stay out of Settings"
        );
        assert_eq!(
            agents[3].built_in, None,
            "a failed lookup leaves the flag unknown"
        );
        assert!(agents[3].is_manageable());

        let envelope = client
            .agent_entity("reviewer", "/repo")
            .await
            .expect("entity");
        assert_eq!(envelope.source, AgentSource::Markdown);
        assert_eq!(envelope.config.steps, Some(12));
        assert_eq!(envelope.config.mode, Some(AgentMode::Subagent));
        assert_eq!(envelope.config.permissions.len(), 1);
        assert_eq!(
            envelope.config.request,
            Some(json!({"headers": {"x": "y"}, "body": {"temperature": 0.3}}))
        );
        assert!(client.agent_entity("broken", "/repo").await.is_err());
        let permissions = client
            .agent_permissions("reviewer", "/repo")
            .await
            .expect("permissions");
        assert_eq!(permissions.global[0].effect, PermissionEffect::Ask);
        assert_eq!(permissions.agent[0].effect, PermissionEffect::Deny);
        let disabled = client.disabled_agents("/repo").await.expect("disabled");
        assert_eq!(disabled[0].name, "old");

        client
            .create_agent(
                "team/new",
                &AgentChanges::default(),
                AgentScope::User,
                "/repo",
            )
            .await
            .expect("create");
        client
            .update_agent(
                "reviewer",
                &AgentChanges {
                    disabled: Some(true),
                    ..AgentChanges::default()
                },
                "/repo",
            )
            .await
            .expect("update");
        let refused = client
            .delete_agent("build", Some(AgentScope::User), "/repo")
            .await;
        assert!(matches!(
            &refused,
            Err(error) if error.message() == Some("Agent is built-in")
        ));
        let recorded = recorded.lock().expect("log");
        let writes: Vec<_> = recorded
            .iter()
            .filter(|request| request.method != "GET")
            .collect();
        assert_eq!(writes[0].path, "/api/config/agents/team%2Fnew");
        assert_eq!(writes[1].body, json!({"disabled": true}));
        assert_eq!(writes[2].body, json!({"scope": "user"}));
    }

    #[tokio::test]
    async fn a_failed_list_is_an_error_and_never_an_empty_list() {
        let (client, _) =
            crate::test_server::serve(Box::new(|_| (500, json!({"error": "down"})))).await;
        assert!(client.settings_agents("/repo").await.is_err());
        assert!(client.disabled_agents("/repo").await.is_err());
        assert!(client.agent_permissions("x", "/repo").await.is_err());
    }
}
