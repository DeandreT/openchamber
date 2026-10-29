//! The catalogs the composer selects from, read in one directory.
//!
//! Every read is scoped by `x-opencode-directory`, as `listAgents`,
//! `getProvidersForConfig`, `getConfig` and `listMcpServers` in
//! `packages/ui/src/lib/opencode/client.ts` scope theirs. The records are the
//! flat shapes `openchamber_native_core::send_config` resolves from.

use openchamber_native_core::model_identifier::{self, ModelSelection};
use openchamber_native_core::send_config::{AgentEntry, FlatModel, FlatProvider, ModelChoice};
use reqwest::Url;
use serde_json::{Map, Value};

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::projection;
use crate::wire;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoutingReadiness {
    available: bool,
    auto_ready: bool,
}

impl RoutingReadiness {
    fn state(self) -> openchamber_native_core::send_config::AutoRouting {
        use openchamber_native_core::send_config::AutoRouting;
        if !self.available {
            AutoRouting::Unavailable
        } else if self.auto_ready {
            AutoRouting::Ready
        } else {
            AutoRouting::NotReady
        }
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use openchamber_native_core::send_config::AutoRouting;

    #[test]
    fn readiness_requires_both_authoritative_flags() {
        for (json, expected) in [
            (
                r#"{"available":false,"autoReady":true}"#,
                AutoRouting::Unavailable,
            ),
            (
                r#"{"available":true,"autoReady":false}"#,
                AutoRouting::NotReady,
            ),
            (r#"{"available":true,"autoReady":true}"#, AutoRouting::Ready),
        ] {
            assert_eq!(
                serde_json::from_str::<RoutingReadiness>(json)
                    .expect("valid flags")
                    .state(),
                expected
            );
        }
        for json in [
            "{}",
            r#"{"available":true}"#,
            r#"{"available":true,"autoReady":"true"}"#,
        ] {
            assert!(
                serde_json::from_str::<RoutingReadiness>(json).is_err(),
                "an incomplete answer cannot establish readiness"
            );
        }
    }
}

/// The flat model catalog of one directory and OpenCode's own default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelCatalog {
    pub providers: Vec<FlatProvider>,
    pub models: Vec<FlatModel>,
    /// OpenCode's default model; absent when it has none.
    pub default_model: Option<ModelChoice>,
}

/// The defaults OpenCode's folded config names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenCodeDefaults {
    /// `default_agent`, trimmed.
    pub default_agent: Option<String>,
    /// `model` as `providerID/modelID[#variant]`.
    pub model: Option<String>,
}

impl ServerClient {
    /// OpenChamber's current Auto readiness. A failed read leaves readiness unknown
    /// at the caller; only a declared missing route means unavailable.
    pub async fn auto_routing(
        &self,
    ) -> Result<openchamber_native_core::send_config::AutoRouting, TransportError> {
        use openchamber_native_core::send_config::AutoRouting;
        match self
            .get_json::<RoutingReadiness>(self.endpoint().url("api/routing"), None)
            .await
        {
            Ok(state) => Ok(state.state()),
            Err(TransportError::Http { status: 404 }) => Ok(AutoRouting::Unavailable),
            Err(error) => Err(error),
        }
    }

    /// The URL of an OpenCode route under `api/`.
    pub fn opencode_url(&self, route: &str) -> Url {
        self.endpoint().url(&format!("api/{route}"))
    }

    /// The agents of `directory`, as the UI projects them.
    pub async fn agents(&self, directory: &str) -> Result<Vec<AgentEntry>, TransportError> {
        let agents: wire::Located<Vec<wire::AgentInfo>> = self
            .get_json(self.opencode_url("agent"), Some(directory))
            .await?;
        Ok(agents.data.into_iter().map(projection::agent).collect())
    }

    /// The providers, models and default model of `directory`.
    pub async fn model_catalog(&self, directory: &str) -> Result<ModelCatalog, TransportError> {
        let (models, default_model, providers) = tokio::try_join!(
            self.get_json::<wire::Located<Vec<wire::ModelInfo>>>(
                self.opencode_url("model"),
                Some(directory)
            ),
            self.get_json::<wire::Located<Option<wire::DefaultModel>>>(
                self.opencode_url("model/default"),
                Some(directory)
            ),
            self.get_json::<wire::Located<Vec<wire::ProviderInfo>>>(
                self.opencode_url("provider"),
                Some(directory)
            ),
        )?;
        Ok(ModelCatalog {
            providers: providers
                .data
                .into_iter()
                .map(projection::provider)
                .collect(),
            models: models.data.into_iter().map(projection::model).collect(),
            default_model: default_model.data.map(|model| ModelChoice {
                provider_id: model.provider_id,
                model_id: model.model_id,
                variant: None,
            }),
        })
    }

    /// The defaults of `directory`'s folded OpenCode config.
    pub async fn opencode_defaults(
        &self,
        directory: &str,
    ) -> Result<OpenCodeDefaults, TransportError> {
        let entries: Vec<wire::ConfigEntry> = self
            .get_json(self.opencode_url("config"), Some(directory))
            .await?;
        Ok(opencode_defaults(&fold_config(entries)))
    }

    /// How many MCP servers of `directory` are connected.
    pub async fn connected_mcp_servers(&self, directory: &str) -> Result<usize, TransportError> {
        let servers: wire::Located<Vec<wire::McpServer>> = self
            .get_json(self.opencode_url("mcp"), Some(directory))
            .await?;
        Ok(connected_count(&servers.data))
    }

    /// The URL of the skills OpenChamber discovers for `directory`.
    pub fn skills_url(&self, directory: &str) -> Url {
        let mut url = self.endpoint().url("api/config/skills");
        url.query_pairs_mut().append_pair("directory", directory);
        url
    }

    /// How many skills `directory` has, external ones not counted, as the
    /// Work status card counts them.
    pub async fn skill_count(&self, directory: &str) -> Result<usize, TransportError> {
        let skills: wire::Skills = self
            .get_json(self.skills_url(directory), Some(directory))
            .await?;
        Ok(skills.skills.len())
    }
}

/// How many servers report `connected`, as the Work status card counts them.
pub(crate) fn connected_count(servers: &[wire::McpServer]) -> usize {
    servers
        .iter()
        .filter(|server| server.status.status == "connected")
        .count()
}

/// Folds config documents lowest priority first, as `mergeConfigDocuments`
/// does: a later value replaces an earlier one, and two objects under one
/// key merge shallowly.
pub(crate) fn fold_config(entries: Vec<wire::ConfigEntry>) -> Map<String, Value> {
    let mut merged = Map::new();
    for entry in entries {
        if entry.kind != "document" {
            continue;
        }
        for (key, value) in entry.info.into_iter().flatten() {
            match (merged.get_mut(&key), value) {
                (Some(Value::Object(previous)), Value::Object(value)) => previous.extend(value),
                (_, value) => {
                    merged.insert(key, value);
                }
            }
        }
    }
    merged
}

/// The folded config's `default_agent` and `model`, as the web client's
/// config loader reads them through `configModelIdentifier`.
pub(crate) fn opencode_defaults(config: &Map<String, Value>) -> OpenCodeDefaults {
    let default_agent = config
        .get("default_agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|agent| !agent.is_empty())
        .map(str::to_owned);
    let model = match config.get("model") {
        Some(Value::String(model)) => model_identifier::parse_model_selection(model, None)
            .and_then(|selection| model_identifier::format_model_selection(&selection)),
        Some(Value::Object(model)) => {
            let text = |key: &str| model.get(key).and_then(Value::as_str);
            match (text("providerID"), text("model")) {
                (Some(provider_id), Some(model_id)) => {
                    model_identifier::format_model_selection(&ModelSelection {
                        provider_id: provider_id.to_owned(),
                        model_id: model_id.to_owned(),
                        variant: text("variant").map(str::to_owned),
                    })
                }
                _ => None,
            }
        }
        _ => None,
    };
    OpenCodeDefaults {
        default_agent,
        model,
    }
}
