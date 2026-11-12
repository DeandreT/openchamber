//! Settings → MCP (`useMcpConfigStore.ts`, `useMcpStore.ts`,
//! `ProviderOAuthMethods.tsx`): the MCP servers a directory's config files
//! define (`/api/config/mcp`), each server's status with the reason OpenCode
//! gives for it (`GET /api/mcp`), the OAuth sign-in OpenCode registers for a
//! remote server (`/api/integration/:id/connect/oauth`) and the
//! `integration.use` policies that block a server (`GET /api/config`).
//!
//! Every call names the directory it works in, as the status calls in
//! [`crate::mcp`] do.

use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, encode_directory};
use crate::error::TransportError;
use crate::files::read_answer;
use crate::mcp::{McpConnection, McpServerStatus};
use crate::wire;

/// Where a server's config entry lives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum McpScope {
    /// The user's global configuration.
    #[default]
    User,
    Project,
}

impl McpScope {
    fn as_wire(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// How OpenCode reaches a server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum McpKind {
    /// A command OpenCode spawns.
    #[default]
    Local,
    /// A hosted server behind a URL.
    Remote,
}

/// How OpenCode opens the MCP connection. An absent key is `Legacy`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum McpProtocol {
    #[default]
    Legacy,
    Auto,
    Revision20260728,
}

impl McpProtocol {
    pub const ALL: [Self; 3] = [Self::Legacy, Self::Auto, Self::Revision20260728];

    /// The config spelling.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Auto => "auto",
            Self::Revision20260728 => "2026-07-28",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|protocol| protocol.as_wire() == value)
    }
}

/// The OAuth block of a remote server's entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum McpOAuthSetting {
    /// No `oauth` key: OpenCode decides.
    #[default]
    Unset,
    /// `oauth: false`.
    Disabled,
    Configured(McpOAuth),
}

/// OAuth fields as OpenCode 2 spells them in config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct McpOAuth {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scope: Option<String>,
    pub callback_port: Option<u64>,
    pub redirect_uri: Option<String>,
    pub auth_server_metadata_url: Option<String>,
}

/// One server a config file defines, as `GET /api/config/mcp` lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub kind: McpKind,
    /// The argv of a local server.
    pub command: Vec<String>,
    /// The address of a remote server.
    pub url: String,
    pub environment: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub oauth: McpOAuthSetting,
    pub disabled: bool,
    /// `None` leaves Code Mode to OpenCode's own per-server default.
    pub codemode: Option<bool>,
    /// Milliseconds; `startup` applies to a local server only.
    pub timeout_startup: Option<u64>,
    pub timeout_catalog: Option<u64>,
    pub timeout_execution: Option<u64>,
    pub protocol: McpProtocol,
    pub scope: Option<McpScope>,
    /// The entry still uses OpenCode 1 spellings.
    pub legacy: bool,
    /// The config file the entry lives in, when the server names it.
    pub path: Option<String>,
}

#[derive(Deserialize)]
struct ConfigWire {
    name: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    command: Vec<String>,
    url: Option<String>,
    #[serde(default)]
    environment: Map<String, Value>,
    #[serde(default)]
    headers: Map<String, Value>,
    oauth: Option<Value>,
    disabled: Option<bool>,
    codemode: Option<bool>,
    timeout: Option<TimeoutWire>,
    protocol: Option<String>,
    scope: Option<String>,
    #[serde(default)]
    legacy: bool,
    path: Option<String>,
}

#[derive(Default, Deserialize)]
struct TimeoutWire {
    startup: Option<u64>,
    catalog: Option<u64>,
    execution: Option<u64>,
}

fn pairs(record: Map<String, Value>) -> Vec<(String, String)> {
    record
        .into_iter()
        .filter_map(|(key, value)| Some((key, value.as_str()?.to_owned())))
        .collect()
}

impl From<ConfigWire> for McpServerConfig {
    fn from(wire: ConfigWire) -> Self {
        let timeout = wire.timeout.unwrap_or_default();
        Self {
            name: wire.name,
            kind: if wire.kind.as_deref() == Some("remote") {
                McpKind::Remote
            } else {
                McpKind::Local
            },
            command: wire.command,
            url: wire.url.unwrap_or_default(),
            environment: pairs(wire.environment),
            headers: pairs(wire.headers),
            oauth: match wire.oauth {
                Some(Value::Bool(false)) => McpOAuthSetting::Disabled,
                Some(value @ Value::Object(_)) => serde_json::from_value(value)
                    .map(McpOAuthSetting::Configured)
                    .unwrap_or_default(),
                _ => McpOAuthSetting::Unset,
            },
            disabled: wire.disabled == Some(true),
            codemode: wire.codemode,
            timeout_startup: timeout.startup,
            timeout_catalog: timeout.catalog,
            timeout_execution: timeout.execution,
            protocol: wire
                .protocol
                .as_deref()
                .and_then(McpProtocol::from_wire)
                .unwrap_or_default(),
            scope: match wire.scope.as_deref() {
                Some("project") => Some(McpScope::Project),
                Some("user") => Some(McpScope::User),
                _ => None,
            },
            legacy: wire.legacy,
            path: wire.path,
        }
    }
}

/// Everything a create or a save writes into a server's entry (`McpDraft`).
/// Number fields are the form's text: one that names no positive number is
/// left out of the entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpDraft {
    pub scope: McpScope,
    pub kind: McpKind,
    pub command: Vec<String>,
    pub url: String,
    pub environment: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    /// `false` writes `oauth: false`.
    pub oauth_enabled: bool,
    pub oauth_client_id: String,
    pub oauth_client_secret: String,
    pub oauth_scope: String,
    pub oauth_redirect_uri: String,
    pub oauth_callback_port: String,
    pub oauth_auth_server_metadata_url: String,
    pub protocol: McpProtocol,
    pub timeout_startup: String,
    pub timeout_catalog: String,
    pub timeout_execution: String,
    pub codemode: Option<bool>,
    pub disabled: bool,
}

/// A form field's positive whole number (`positiveInteger`).
fn positive_integer(value: &str) -> Option<u64> {
    let parsed: f64 = value.trim().parse().ok()?;
    (parsed.is_finite() && parsed > 0.0).then(|| parsed.floor() as u64)
}

/// The rows as a config record: blank keys dropped, keys trimmed, a key
/// listed twice keeps its last value (`envArrayToRecord`).
fn record(rows: &[(String, String)]) -> Value {
    let mut record = Map::new();
    for (key, value) in rows {
        let key = key.trim();
        if !key.is_empty() {
            record.insert(key.to_owned(), json!(value));
        }
    }
    Value::Object(record)
}

impl McpDraft {
    /// The request body (`buildMcpBody` for a complete draft). The `oauth`
    /// and `timeout` blocks are rebuilt whole, so an emptied field drops its
    /// key; `codemode` and `protocol` at their default remove theirs.
    fn body(&self) -> Map<String, Value> {
        let mut body = Map::new();
        body.insert("scope".to_owned(), json!(self.scope.as_wire()));
        body.insert(
            "type".to_owned(),
            json!(match self.kind {
                McpKind::Local => "local",
                McpKind::Remote => "remote",
            }),
        );
        let command: Vec<&String> = self
            .command
            .iter()
            .filter(|argument| !argument.trim().is_empty())
            .collect();
        body.insert("command".to_owned(), json!(command));
        body.insert("url".to_owned(), json!(self.url.trim()));
        body.insert("environment".to_owned(), record(&self.environment));
        body.insert("headers".to_owned(), record(&self.headers));
        body.insert(
            "oauth".to_owned(),
            if self.oauth_enabled {
                let mut oauth = Map::new();
                for (key, value) in [
                    ("client_id", &self.oauth_client_id),
                    ("client_secret", &self.oauth_client_secret),
                    ("scope", &self.oauth_scope),
                    ("redirect_uri", &self.oauth_redirect_uri),
                ] {
                    if !value.trim().is_empty() {
                        oauth.insert(key.to_owned(), json!(value.trim()));
                    }
                }
                if let Some(port) = positive_integer(&self.oauth_callback_port) {
                    oauth.insert("callback_port".to_owned(), json!(port));
                }
                let metadata = self.oauth_auth_server_metadata_url.trim();
                if !metadata.is_empty() {
                    oauth.insert("auth_server_metadata_url".to_owned(), json!(metadata));
                }
                Value::Object(oauth)
            } else {
                Value::Bool(false)
            },
        );
        let mut timeout = Map::new();
        if self.kind == McpKind::Local
            && let Some(startup) = positive_integer(&self.timeout_startup)
        {
            timeout.insert("startup".to_owned(), json!(startup));
        }
        if let Some(catalog) = positive_integer(&self.timeout_catalog) {
            timeout.insert("catalog".to_owned(), json!(catalog));
        }
        if let Some(execution) = positive_integer(&self.timeout_execution) {
            timeout.insert("execution".to_owned(), json!(execution));
        }
        body.insert(
            "timeout".to_owned(),
            if timeout.is_empty() {
                Value::Null
            } else {
                Value::Object(timeout)
            },
        );
        body.insert("codemode".to_owned(), json!(self.codemode));
        body.insert("disabled".to_owned(), json!(self.disabled));
        body.insert(
            "protocol".to_owned(),
            match self.protocol {
                McpProtocol::Legacy => Value::Null,
                protocol => json!(protocol.as_wire()),
            },
        );
        body
    }
}

/// Why an MCP config write or a sign-in step did not land.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum McpConfigError {
    #[error("the server refused the request (HTTP {status})")]
    Refused {
        status: u16,
        /// The server's own reason, when it gave one.
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl McpConfigError {
    /// The server's reason for a refusal.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Refused { message, .. } => message.as_deref(),
            Self::Transport(_) => None,
        }
    }
}

/// One server's status with the reason OpenCode gives for a failed or
/// unauthorized one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerReport {
    pub status: McpServerStatus,
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct ReportWire {
    name: String,
    status: ReportStatusWire,
}

#[derive(Deserialize)]
struct ReportStatusWire {
    status: String,
    error: Option<String>,
}

/// The OAuth sign-in OpenCode registered for a remote server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpOAuthIntegration {
    pub id: String,
    /// The integration's `oauth` methods, in its order.
    pub methods: Vec<McpOAuthMethod>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpOAuthMethod {
    pub id: String,
    pub label: Option<String>,
    /// The method asks questions before it starts.
    pub has_form: bool,
}

#[derive(Deserialize)]
struct IntegrationWire {
    id: String,
    name: Option<String>,
    metadata: Option<Value>,
    #[serde(default)]
    methods: Vec<MethodWire>,
}

#[derive(Deserialize)]
struct MethodWire {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    label: Option<String>,
    #[serde(default)]
    form: Vec<Value>,
}

/// What the user does next in a started sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthMode {
    /// The browser finishes it; the attempt is polled.
    Auto,
    /// The user pastes a code out of the browser.
    Code,
}

/// A started sign-in (`integration.oauth.connect`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthAttempt {
    pub id: String,
    pub mode: OAuthMode,
    pub url: String,
    pub instructions: String,
}

#[derive(Deserialize)]
struct AttemptWire {
    #[serde(rename = "attemptID")]
    attempt_id: String,
    mode: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    instructions: String,
}

/// Where a polled sign-in stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OAuthProgress {
    Pending,
    Complete,
    Failed(String),
}

#[derive(Deserialize)]
struct ProgressWire {
    status: String,
    #[serde(default)]
    message: String,
}

/// One `integration.use` statement of a config document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationPolicy {
    /// `mcp:<server>` or `skill:<id>`, with `*` and `?` wildcards.
    pub resource: String,
    pub deny: bool,
}

/// The `integration.use` statements of every config document in the order
/// OpenCode decides by: documents reversed, the last matching statement wins
/// (`readIntegrationPolicies`).
fn integration_policies(entries: &[Value]) -> Vec<IntegrationPolicy> {
    let mut policies = Vec::new();
    for entry in entries.iter().rev() {
        if entry["type"] != "document" {
            continue;
        }
        let info = &entry["info"];
        for list in [&info["experimental"]["policies"], &info["policies"]] {
            for policy in list.as_array().into_iter().flatten() {
                if policy["action"] != "integration.use" {
                    continue;
                }
                let (Some(resource), Some(effect)) =
                    (policy["resource"].as_str(), policy["effect"].as_str())
                else {
                    continue;
                };
                policies.push(IntegrationPolicy {
                    resource: resource.to_owned(),
                    deny: effect == "deny",
                });
            }
        }
    }
    policies
}

/// OpenCode's `Wildcard.match`: `*` is any run, `?` one character, and a
/// trailing ` *` also matches nothing.
fn matches_wildcard(input: &str, pattern: &str) -> bool {
    fn run(input: &[char], pattern: &[char]) -> bool {
        match pattern.split_first() {
            None => input.is_empty(),
            Some(('*', rest)) => (0..=input.len()).any(|skipped| run(&input[skipped..], rest)),
            Some(('?', rest)) => !input.is_empty() && run(&input[1..], rest),
            Some((expected, rest)) => input.first() == Some(expected) && run(&input[1..], rest),
        }
    }
    let input: Vec<char> = input.replace('\\', "/").chars().collect();
    let pattern = pattern.replace('\\', "/");
    let whole: Vec<char> = pattern.chars().collect();
    if run(&input, &whole) {
        return true;
    }
    pattern.strip_suffix(" *").is_some_and(|head| {
        let head: Vec<char> = head.chars().collect();
        run(&input, &head)
    })
}

/// The `message` of a config mutation's answer (`buildAppliedResponse`).
fn applied_message(answer: &[u8]) -> Option<String> {
    let answer: Value = serde_json::from_slice(answer).ok()?;
    Some(answer["message"].as_str()?.to_owned()).filter(|message| !message.is_empty())
}

/// Whether OpenCode drops `resource` by policy (`isIntegrationDenied`).
pub fn is_integration_denied(policies: &[IntegrationPolicy], resource: &str) -> bool {
    policies
        .iter()
        .rev()
        .find(|policy| matches_wildcard(resource, &policy.resource))
        .is_some_and(|policy| policy.deny)
}

impl ServerClient {
    fn integration_url(&self, integration: &str, tail: &[&str]) -> Url {
        let mut url = self.opencode_url("integration/");
        {
            let mut segments = url
                .path_segments_mut()
                .expect("HTTP URLs have path segments");
            segments
                .pop_if_empty()
                .push(integration)
                .push("connect")
                .push("oauth");
            for segment in tail {
                segments.push(segment);
            }
        }
        url
    }

    async fn mcp_request(
        &self,
        method: Method,
        url: Url,
        directory: &str,
        body: Option<Value>,
    ) -> Result<Vec<u8>, McpConfigError> {
        let mut request = self
            .authorized(method, url)
            .header("x-opencode-directory", encode_directory(directory));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(TransportError::from_request)?;
        match read_answer(response).await? {
            Ok(bytes) => Ok(bytes),
            Err(refusal) => Err(McpConfigError::Refused {
                status: refusal.status.as_u16(),
                message: refusal.message,
            }),
        }
    }

    /// The servers `directory`'s config files define. A failure is an error,
    /// never an empty list.
    pub async fn mcp_configs(
        &self,
        directory: &str,
    ) -> Result<Vec<McpServerConfig>, TransportError> {
        let configs: Vec<ConfigWire> = self
            .get_json(self.mcp_config_url(None, directory), Some(directory))
            .await?;
        Ok(configs.into_iter().map(McpServerConfig::from).collect())
    }

    /// Creates server `name` from `draft` in the draft's scope. Answers with
    /// the server's own sentence about what it did, when it sent one.
    pub async fn create_mcp_config(
        &self,
        name: &str,
        draft: &McpDraft,
        directory: &str,
    ) -> Result<Option<String>, McpConfigError> {
        let url = self.mcp_config_url(Some(name), directory);
        self.mcp_request(
            Method::POST,
            url,
            directory,
            Some(Value::Object(draft.body())),
        )
        .await
        .map(|answer| applied_message(&answer))
    }

    /// Rewrites server `name`'s entry from `draft`, in the file that holds it.
    pub async fn update_mcp_config(
        &self,
        name: &str,
        draft: &McpDraft,
        directory: &str,
    ) -> Result<(), McpConfigError> {
        let url = self.mcp_config_url(Some(name), directory);
        self.mcp_request(
            Method::PATCH,
            url,
            directory,
            Some(Value::Object(draft.body())),
        )
        .await
        .map(drop)
    }

    /// Removes server `name` from the config file that defines it. Answers
    /// with the server's own sentence about what it did, when it sent one.
    pub async fn delete_mcp_config(
        &self,
        name: &str,
        directory: &str,
    ) -> Result<Option<String>, McpConfigError> {
        let url = self.mcp_config_url(Some(name), directory);
        self.mcp_request(Method::DELETE, url, directory, None)
            .await
            .map(|answer| applied_message(&answer))
    }

    /// The servers of `directory` with their status and its reason, in the
    /// server's order. A failure is an error, never an empty list.
    pub async fn mcp_server_reports(
        &self,
        directory: &str,
    ) -> Result<Vec<McpServerReport>, TransportError> {
        let servers: wire::Located<Vec<ReportWire>> = self
            .get_json(self.opencode_url("mcp"), Some(directory))
            .await?;
        Ok(servers
            .data
            .into_iter()
            .map(|server| McpServerReport {
                status: McpServerStatus {
                    connection: McpConnection::from_wire(&server.status.status),
                    name: server.name,
                },
                error: server.status.error.filter(|error| !error.trim().is_empty()),
            })
            .collect())
    }

    /// The OAuth sign-in OpenCode registered for remote server `name`, or
    /// `None` when it has none yet. The integration's id is upstream's hash,
    /// so the match goes by source and name (`findMcpIntegration`).
    pub async fn mcp_oauth_integration(
        &self,
        name: &str,
        directory: &str,
    ) -> Result<Option<McpOAuthIntegration>, TransportError> {
        let integrations: wire::Located<Vec<IntegrationWire>> = self
            .get_json(self.opencode_url("integration"), Some(directory))
            .await?;
        let found = integrations.data.into_iter().find(|integration| {
            integration.id.starts_with("mcp_")
                && integration
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata["source"] == "mcp")
                && integration.name.as_deref() == Some(name)
        });
        Ok(found.and_then(|integration| {
            let methods: Vec<McpOAuthMethod> = integration
                .methods
                .into_iter()
                .filter(|method| method.kind.as_deref() == Some("oauth"))
                .filter_map(|method| {
                    Some(McpOAuthMethod {
                        id: method.id?,
                        label: method.label,
                        has_form: method.form.iter().any(|field| field["type"] != "external"),
                    })
                })
                .collect();
            (!methods.is_empty()).then_some(McpOAuthIntegration {
                id: integration.id,
                methods,
            })
        }))
    }

    /// Starts a sign-in with `method` (`integration.oauth.connect`).
    pub async fn start_mcp_oauth(
        &self,
        integration: &str,
        method: &str,
        directory: &str,
    ) -> Result<OAuthAttempt, McpConfigError> {
        let bytes = self
            .mcp_request(
                Method::POST,
                self.integration_url(integration, &[]),
                directory,
                Some(json!({ "methodID": method })),
            )
            .await?;
        let attempt: wire::Located<AttemptWire> = serde_json::from_slice(&bytes)
            .map_err(|error| TransportError::Malformed(error.to_string()))?;
        Ok(OAuthAttempt {
            id: attempt.data.attempt_id,
            mode: if attempt.data.mode == "code" {
                OAuthMode::Code
            } else {
                OAuthMode::Auto
            },
            url: attempt.data.url,
            instructions: attempt.data.instructions,
        })
    }

    /// Where a browser sign-in stands (`integration.oauth.status`).
    pub async fn mcp_oauth_progress(
        &self,
        integration: &str,
        attempt: &str,
        directory: &str,
    ) -> Result<OAuthProgress, McpConfigError> {
        let bytes = self
            .mcp_request(
                Method::GET,
                self.integration_url(integration, &[attempt]),
                directory,
                None,
            )
            .await?;
        let progress: wire::Located<ProgressWire> = serde_json::from_slice(&bytes)
            .map_err(|error| TransportError::Malformed(error.to_string()))?;
        Ok(match progress.data.status.as_str() {
            "complete" => OAuthProgress::Complete,
            "failed" => OAuthProgress::Failed(progress.data.message),
            _ => OAuthProgress::Pending,
        })
    }

    /// Hands a pasted code to a sign-in (`integration.oauth.complete`).
    pub async fn complete_mcp_oauth(
        &self,
        integration: &str,
        attempt: &str,
        code: &str,
        directory: &str,
    ) -> Result<(), McpConfigError> {
        self.mcp_request(
            Method::POST,
            self.integration_url(integration, &[attempt, "complete"]),
            directory,
            Some(json!({ "code": code })),
        )
        .await
        .map(drop)
    }

    /// Drops a sign-in the user walked away from (`integration.oauth.cancel`).
    pub async fn cancel_mcp_oauth(
        &self,
        integration: &str,
        attempt: &str,
        directory: &str,
    ) -> Result<(), McpConfigError> {
        self.mcp_request(
            Method::DELETE,
            self.integration_url(integration, &[attempt]),
            directory,
            None,
        )
        .await
        .map(drop)
    }

    /// The `integration.use` policies of `directory`'s config documents.
    pub async fn integration_policies(
        &self,
        directory: &str,
    ) -> Result<Vec<IntegrationPolicy>, TransportError> {
        let entries: Vec<Value> = self
            .get_json(self.opencode_url("config"), Some(directory))
            .await?;
        Ok(integration_policies(&entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> McpDraft {
        McpDraft {
            oauth_enabled: true,
            ..McpDraft::default()
        }
    }

    #[test]
    fn a_draft_writes_the_reference_body() {
        let local = McpDraft {
            command: vec!["npx".into(), " ".into(), "-y".into()],
            environment: vec![
                (" KEY ".into(), "one".into()),
                (String::new(), "dropped".into()),
                ("KEY".into(), "two".into()),
            ],
            timeout_startup: " 5000.7 ".into(),
            timeout_catalog: "-1".into(),
            timeout_execution: "abc".into(),
            ..draft()
        };
        assert_eq!(
            Value::Object(local.body()),
            json!({
                "scope": "user",
                "type": "local",
                "command": ["npx", "-y"],
                "url": "",
                "environment": {"KEY": "two"},
                "headers": {},
                "oauth": {},
                "timeout": {"startup": 5000},
                "codemode": null,
                "disabled": false,
                "protocol": null,
            })
        );
        let remote = McpDraft {
            scope: McpScope::Project,
            kind: McpKind::Remote,
            url: " https://mcp.test ".into(),
            headers: vec![("Authorization".into(), "Bearer x".into())],
            oauth_client_id: " id ".into(),
            oauth_callback_port: "8123".into(),
            oauth_auth_server_metadata_url: "https://auth.test/meta".into(),
            protocol: McpProtocol::Revision20260728,
            timeout_startup: "5000".into(),
            timeout_execution: "30000".into(),
            codemode: Some(false),
            disabled: true,
            ..draft()
        };
        let body = Value::Object(remote.body());
        assert_eq!(body["scope"], "project");
        assert_eq!(body["url"], "https://mcp.test");
        assert_eq!(
            body["oauth"],
            json!({
                "client_id": "id",
                "callback_port": 8123,
                "auth_server_metadata_url": "https://auth.test/meta",
            })
        );
        assert_eq!(
            body["timeout"],
            json!({"execution": 30000}),
            "a remote server has no startup timeout"
        );
        assert_eq!(body["codemode"], false);
        assert_eq!(body["protocol"], "2026-07-28");
        let no_oauth = McpDraft {
            oauth_enabled: false,
            oauth_client_id: "kept out".into(),
            ..draft()
        };
        assert_eq!(no_oauth.body()["oauth"], false);
        assert_eq!(no_oauth.body()["timeout"], Value::Null);
    }

    #[test]
    fn policies_decide_by_the_last_matching_statement() {
        let entries = vec![
            json!({"type": "document", "info": {"policies": [
                {"action": "integration.use", "resource": "mcp:*", "effect": "deny"},
                {"action": "provider.use", "resource": "*", "effect": "deny"},
            ]}}),
            json!({"type": "other", "info": {"policies": [
                {"action": "integration.use", "resource": "*", "effect": "deny"},
            ]}}),
            json!({"type": "document", "info": {
                "experimental": {"policies": [
                    {"action": "integration.use", "resource": "mcp:allowed", "effect": "allow"},
                ]},
            }}),
        ];
        let policies = integration_policies(&entries);
        assert_eq!(policies.len(), 2);
        // Documents are reversed, so the first document's statement is last.
        assert!(is_integration_denied(&policies, "mcp:allowed"));
        assert!(is_integration_denied(&policies, "mcp:other"));
        assert!(!is_integration_denied(&policies, "skill:any"));
        assert!(!is_integration_denied(&[], "mcp:any"));
        assert!(matches_wildcard("mcp:git", "mcp:g?t"));
        assert!(matches_wildcard("run", "run *"));
        assert!(matches_wildcard("run now", "run *"));
        assert!(!matches_wildcard("runner", "run *"));
    }

    #[tokio::test]
    async fn configs_parse_and_writes_name_the_server_and_directory() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/api/config/mcp") => (
                    200,
                    json!([
                        {"name": "files", "type": "local", "command": ["npx", "files"],
                         "environment": {"TOKEN": "t"}, "timeout": {"startup": 5000},
                         "scope": "project", "legacy": true},
                        {"name": "hosted", "type": "remote", "url": "https://mcp.test",
                         "oauth": false, "disabled": true, "codemode": true,
                         "protocol": "auto", "scope": "user"},
                        {"name": "signed", "type": "remote", "url": "https://signed.test",
                         "oauth": {"client_id": "id", "callback_port": 9000}},
                    ]),
                ),
                ("POST", _) => (500, json!({"error": "MCP server \"files\" already exists"})),
                ("DELETE", _) => (
                    200,
                    json!({"success": true, "message": "MCP server \"files\" deleted."}),
                ),
                _ => (200, json!({"success": true})),
            }
        }))
        .await;
        let configs = client.mcp_configs("/repo").await.expect("configs");
        assert_eq!(configs.len(), 3);
        assert_eq!(configs[0].kind, McpKind::Local);
        assert_eq!(configs[0].environment, [("TOKEN".into(), "t".into())]);
        assert_eq!(configs[0].timeout_startup, Some(5000));
        assert_eq!(configs[0].scope, Some(McpScope::Project));
        assert!(configs[0].legacy && !configs[0].disabled);
        assert_eq!(configs[1].oauth, McpOAuthSetting::Disabled);
        assert!(configs[1].disabled);
        assert_eq!(configs[1].codemode, Some(true));
        assert_eq!(configs[1].protocol, McpProtocol::Auto);
        assert!(matches!(
            &configs[2].oauth,
            McpOAuthSetting::Configured(oauth)
                if oauth.client_id.as_deref() == Some("id") && oauth.callback_port == Some(9000)
        ));

        let refused = client.create_mcp_config("files", &draft(), "/repo").await;
        assert!(matches!(
            &refused,
            Err(error) if error.message() == Some("MCP server \"files\" already exists")
        ));
        client
            .update_mcp_config("files", &draft(), "/repo")
            .await
            .expect("update");
        assert_eq!(
            client
                .delete_mcp_config("files", "/repo")
                .await
                .expect("delete")
                .as_deref(),
            Some("MCP server \"files\" deleted.")
        );
        let recorded = recorded.lock().expect("log");
        let sent: Vec<String> = recorded
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect();
        assert_eq!(
            sent,
            [
                "GET /api/config/mcp",
                "POST /api/config/mcp/files",
                "PATCH /api/config/mcp/files",
                "DELETE /api/config/mcp/files",
            ]
        );
        assert_eq!(recorded[2].body["type"], "local");
    }

    #[tokio::test]
    async fn a_failed_read_is_an_error_and_never_an_empty_answer() {
        let (client, _) =
            crate::test_server::serve(Box::new(|_| (500, json!({"error": "down"})))).await;
        assert!(client.mcp_configs("/repo").await.is_err());
        assert!(client.mcp_server_reports("/repo").await.is_err());
        assert!(client.integration_policies("/repo").await.is_err());
        assert!(client.mcp_oauth_integration("x", "/repo").await.is_err());
    }

    #[tokio::test]
    async fn the_sign_in_follows_the_integration_routes() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/api/mcp") => (
                    200,
                    json!({"location": {}, "data": [
                        {"name": "hosted", "status": {"status": "needs_auth", "error": "sign in"}},
                        {"name": "ok", "status": {"status": "connected"}},
                    ]}),
                ),
                ("GET", "/api/integration") => (
                    200,
                    json!({"location": {}, "data": [
                        {"id": "github", "name": "hosted", "methods": [{"type": "oauth", "id": "x"}]},
                        {"id": "mcp_abc", "name": "hosted", "metadata": {"source": "mcp"},
                         "methods": [
                            {"type": "key", "id": "key"},
                            {"type": "oauth", "id": "browser", "label": "Browser"},
                         ]},
                    ]}),
                ),
                ("POST", "/api/integration/mcp_abc/connect/oauth") => (
                    200,
                    json!({"location": {}, "data": {
                        "attemptID": "try 1", "mode": "auto",
                        "url": "https://auth.test/start", "instructions": "",
                    }}),
                ),
                ("GET", _) => (200, json!({"location": {}, "data": {"status": "pending"}})),
                _ => (200, Value::Null),
            }
        }))
        .await;
        let reports = client.mcp_server_reports("/repo").await.expect("reports");
        assert_eq!(reports[0].status.connection, McpConnection::NeedsAuth);
        assert_eq!(reports[0].error.as_deref(), Some("sign in"));
        assert_eq!(reports[1].error, None);
        let integration = client
            .mcp_oauth_integration("hosted", "/repo")
            .await
            .expect("integrations")
            .expect("the MCP integration");
        assert_eq!(integration.id, "mcp_abc");
        assert_eq!(integration.methods.len(), 1);
        assert!(!integration.methods[0].has_form);
        assert_eq!(
            client
                .mcp_oauth_integration("other", "/repo")
                .await
                .expect("integrations"),
            None
        );
        let attempt = client
            .start_mcp_oauth("mcp_abc", "browser", "/repo")
            .await
            .expect("attempt");
        assert_eq!(attempt.mode, OAuthMode::Auto);
        assert_eq!(
            client
                .mcp_oauth_progress("mcp_abc", &attempt.id, "/repo")
                .await
                .expect("progress"),
            OAuthProgress::Pending
        );
        client
            .complete_mcp_oauth("mcp_abc", &attempt.id, "code", "/repo")
            .await
            .expect("complete");
        client
            .cancel_mcp_oauth("mcp_abc", &attempt.id, "/repo")
            .await
            .expect("cancel");
        let recorded = recorded.lock().expect("log");
        let tail: Vec<String> = recorded[3..]
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect();
        assert_eq!(
            tail,
            [
                "POST /api/integration/mcp_abc/connect/oauth",
                "GET /api/integration/mcp_abc/connect/oauth/try%201",
                "POST /api/integration/mcp_abc/connect/oauth/try%201/complete",
                "DELETE /api/integration/mcp_abc/connect/oauth/try%201",
            ]
        );
        assert_eq!(recorded[3].body, json!({"methodID": "browser"}));
        assert_eq!(recorded[5].body, json!({"code": "code"}));
    }
}
