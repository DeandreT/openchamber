//! Settings → Providers (`ProvidersPage.tsx`, `providerAuth.ts`,
//! `integration-catalog.ts`): the providers OpenCode lists with their models,
//! the integrations that say how each one signs in and what is connected, and
//! the writes that connect, disconnect and define a provider.
//!
//! OpenCode owns every credential. A key goes from the caller straight into
//! the body of `POST /api/integration/:id/connect/key` and nowhere else: it
//! is never logged, formatted or kept by this module, and no error carries
//! it. In enterprise mode the server refuses every way in (403 with
//! `code: enterprise_mode`), which callers receive as
//! [`ProviderError::EnterpriseMode`]; removing a stored credential stays
//! allowed.

use std::collections::HashSet;
use std::fmt;

use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, encode_directory};
use crate::error::TransportError;
use crate::files::{Refusal, parse, read_answer};
use crate::{projection, wire};

/// `parseModelRefs`' bound on stored model references.
const MODEL_REF_LIMIT: usize = 1024;

/// An API key on its way to OpenCode. It has no `Display`, and `Debug` prints
/// no part of it, so it cannot reach a log or an error by formatting.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey(String);

impl SecretKey {
    /// The trimmed key, or `None` for blank text.
    pub fn new(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| Self(text.to_owned()))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretKey(..)")
    }
}

/// One model of a listed provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedModel {
    /// The catalog key, which model visibility is stored by.
    pub id: String,
    pub name: Option<String>,
    pub context_limit: Option<u64>,
}

/// A provider OpenCode lists and has not disabled, with its enabled models.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedProvider {
    pub id: String,
    pub name: Option<String>,
    pub models: Vec<ListedModel>,
}

/// How a stored credential was created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialMethod {
    Key,
    OAuth,
    External,
}

/// What gives an integration its credentials right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Connection {
    /// A login OpenCode stores; the only kind that can be removed.
    Credential {
        id: String,
        label: String,
        method: CredentialMethod,
        /// OpenCode asks the user to sign in again.
        needs_auth: bool,
    },
    /// A variable the server's environment provides.
    Environment { name: String, needs_auth: bool },
}

impl Connection {
    /// Whether OpenCode reported `needs_auth` for the connection.
    pub fn needs_auth(&self) -> bool {
        match self {
            Self::Credential { needs_auth, .. } | Self::Environment { needs_auth, .. } => {
                *needs_auth
            }
        }
    }
}

/// Which kind of button a sign-in method is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignInKind {
    /// A browser or device-code sign-in followed through an attempt.
    OAuth,
    /// A reference to credentials managed outside OpenCode.
    External,
}

/// A method that signs in with a button instead of a pasted key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignInMethod {
    pub id: String,
    /// The method's own label; `None` when it declares none.
    pub label: Option<String>,
    pub kind: SignInKind,
    /// The method's form has a required field, which only a client that
    /// renders the form can answer.
    pub needs_answers: bool,
}

/// One integration of `GET /api/integration`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Integration {
    pub id: String,
    pub name: String,
    /// Whether it accepts a pasted key.
    pub accepts_key: bool,
    /// The key method's form has a required field beside the key.
    pub key_needs_answers: bool,
    /// OAuth and external methods, in declared order.
    pub sign_in: Vec<SignInMethod>,
    pub connections: Vec<Connection>,
    /// The account of a web search provider (`metadata.websearch`), which is
    /// not a model provider.
    pub web_search: bool,
}

impl Integration {
    /// The stored credentials: the connections a user can remove.
    pub fn credentials(&self) -> impl Iterator<Item = &Connection> {
        self.connections
            .iter()
            .filter(|connection| matches!(connection, Connection::Credential { .. }))
    }
}

/// How the client finishes an OAuth attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthMode {
    /// The user finishes in the browser; the attempt is polled.
    Auto,
    /// The user pastes a code out of the browser.
    Code,
}

/// An OAuth attempt OpenCode opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthAttempt {
    pub id: String,
    pub mode: OAuthMode,
    /// The page the user signs in on; empty when the method has none.
    pub url: String,
    pub instructions: String,
}

/// Where a polled attempt stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OAuthStatus {
    Pending,
    Complete,
    Failed { message: String },
    Expired,
}

/// The request format a custom provider speaks (`CUSTOM_PROVIDER_PROTOCOLS`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CustomProtocol {
    #[default]
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
}

impl CustomProtocol {
    fn package(self) -> &'static str {
        match self {
            Self::OpenAiChat => "aisdk:@ai-sdk/openai-compatible",
            Self::OpenAiResponses => "aisdk:@ai-sdk/openai",
            Self::AnthropicMessages => "aisdk:@ai-sdk/anthropic",
        }
    }
}

/// A custom provider as `PUT /api/provider` writes it into the user's
/// OpenCode config. It carries no key: a literal key is stored afterwards
/// through [`ServerClient::connect_provider_key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomProvider {
    pub id: String,
    pub name: String,
    pub protocol: CustomProtocol,
    pub base_url: String,
    /// The environment variable a `{env:VAR}` key named.
    pub env: Option<String>,
    /// `(model ID, display name)` in form order.
    pub models: Vec<(String, String)>,
    /// A key follows this write, so the server may treat the provider as
    /// one with a credential.
    pub has_credential: bool,
}

impl CustomProvider {
    fn body(&self) -> Value {
        let models: Map<String, Value> = self
            .models
            .iter()
            .map(|(id, name)| (id.clone(), json!({ "modelID": id, "name": name })))
            .collect();
        let mut config = Map::new();
        config.insert("package".to_owned(), json!(self.protocol.package()));
        config.insert("name".to_owned(), json!(self.name));
        config.insert("settings".to_owned(), json!({ "baseURL": self.base_url }));
        config.insert("models".to_owned(), Value::Object(models));
        if let Some(env) = &self.env {
            config.insert("env".to_owned(), json!([env]));
        }
        json!({
            "providerID": self.id,
            "config": config,
            "scope": "user",
            "hasCredential": self.has_credential,
        })
    }
}

/// Why a provider write did not land. No variant carries request content.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// The server keeps providers to its OpenCode config (HTTP 403 with
    /// `code: enterprise_mode`).
    #[error("the server refuses new providers and keys in enterprise mode")]
    EnterpriseMode,
    #[error("the server refused the provider change (HTTP {status})")]
    Refused {
        status: u16,
        /// What the server said. Shown only for writes that carry no key.
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl ProviderError {
    fn from_refusal(refusal: Refusal) -> Self {
        if refusal.reason.as_deref() == Some("enterprise_mode") {
            return Self::EnterpriseMode;
        }
        Self::Refused {
            status: refusal.status.as_u16(),
            message: refusal.message,
        }
    }

    /// OpenCode has not registered the integration yet: the answer a key
    /// gets between a custom provider's config write and the file watcher.
    pub fn is_not_found(&self) -> bool {
        match self {
            Self::Refused { status, message } => {
                *status == 404
                    || message
                        .as_deref()
                        .is_some_and(|message| message.to_lowercase().contains("not found"))
            }
            _ => false,
        }
    }
}

#[derive(Deserialize)]
struct FieldWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    required: bool,
}

/// Whether a form has a required field a client must answer. `external`
/// fields only point at a URL and take no answer.
fn needs_answers(form: &[FieldWire]) -> bool {
    form.iter()
        .any(|field| field.kind != "external" && field.required)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum MethodWire {
    Oauth {
        id: String,
        label: Option<String>,
        #[serde(default)]
        form: Vec<FieldWire>,
    },
    Key {
        #[serde(default)]
        form: Vec<FieldWire>,
    },
    External {
        id: String,
        label: Option<String>,
        #[serde(default)]
        form: Vec<FieldWire>,
    },
    /// `command`, `env` and anything newer: not offered here.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct StatusWire {
    status: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ConnectionWire {
    Credential {
        id: String,
        #[serde(default)]
        label: String,
        method: Option<String>,
        status: Option<StatusWire>,
    },
    Env {
        #[serde(default)]
        name: String,
        status: Option<StatusWire>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct IntegrationWire {
    id: String,
    name: Option<String>,
    metadata: Option<Map<String, Value>>,
    #[serde(default)]
    methods: Vec<MethodWire>,
    #[serde(default)]
    connections: Vec<ConnectionWire>,
}

fn integration(wire: IntegrationWire) -> Integration {
    let needs_auth = |status: &Option<StatusWire>| {
        status
            .as_ref()
            .is_some_and(|status| status.status == "needs_auth")
    };
    let mut accepts_key = false;
    let mut key_needs_answers = false;
    let mut sign_in = Vec::new();
    for method in wire.methods {
        let (id, label, form, kind) = match method {
            MethodWire::Key { form } => {
                accepts_key = true;
                key_needs_answers = needs_answers(&form);
                continue;
            }
            MethodWire::Oauth { id, label, form } => (id, label, form, SignInKind::OAuth),
            MethodWire::External { id, label, form } => (id, label, form, SignInKind::External),
            MethodWire::Other => continue,
        };
        sign_in.push(SignInMethod {
            label: label.filter(|label| !label.trim().is_empty()),
            needs_answers: needs_answers(&form),
            id,
            kind,
        });
    }
    let connections = wire
        .connections
        .into_iter()
        .filter_map(|connection| match connection {
            ConnectionWire::Credential {
                id,
                label,
                method,
                status,
            } => Some(Connection::Credential {
                needs_auth: needs_auth(&status),
                label: if label.trim().is_empty() {
                    id.clone()
                } else {
                    label
                },
                method: match method.as_deref() {
                    Some("oauth") => CredentialMethod::OAuth,
                    Some("external") => CredentialMethod::External,
                    _ => CredentialMethod::Key,
                },
                id,
            }),
            ConnectionWire::Env { name, status } => Some(Connection::Environment {
                needs_auth: needs_auth(&status),
                name,
            }),
            ConnectionWire::Other => None,
        })
        .collect();
    Integration {
        name: wire
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| wire.id.clone()),
        web_search: wire
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("websearch"))
            == Some(&Value::Bool(true)),
        id: wire.id,
        accepts_key,
        key_needs_answers,
        sign_in,
        connections,
    }
}

#[derive(Deserialize)]
struct AttemptWire {
    #[serde(rename = "attemptID")]
    attempt_id: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    instructions: String,
    mode: Option<String>,
}

#[derive(Deserialize)]
struct AttemptStatusWire {
    status: String,
    message: Option<String>,
}

/// `parseModelRefs`: trimmed, non-empty, first occurrence of each pair.
fn model_refs(value: Option<&Value>) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    let mut seen = HashSet::new();
    for entry in value.and_then(Value::as_array).into_iter().flatten() {
        let field = |key: &str| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        };
        let (Some(provider_id), Some(model_id)) = (field("providerID"), field("modelID")) else {
            continue;
        };
        if !seen.insert((provider_id.to_owned(), model_id.to_owned())) {
            continue;
        }
        refs.push((provider_id.to_owned(), model_id.to_owned()));
        if refs.len() >= MODEL_REF_LIMIT {
            break;
        }
    }
    refs
}

impl ServerClient {
    fn integration_url(&self, integration_id: &str, tail: &[&str]) -> Url {
        let mut url = self.opencode_url("integration/");
        {
            let mut segments = url
                .path_segments_mut()
                .expect("http and https URLs have path segments");
            segments.pop_if_empty().push(integration_id);
            for segment in tail {
                segments.push(segment);
            }
        }
        url
    }

    /// Sends a provider write and reads its answer. `directory` names the
    /// location the integration list was read from: OpenCode registers
    /// integrations per location, and keeps an OAuth attempt in the one that
    /// started it.
    async fn provider_write(
        &self,
        method: Method,
        url: Url,
        directory: Option<&str>,
        body: Option<Value>,
    ) -> Result<Vec<u8>, ProviderError> {
        let mut request = self.authorized(method, url);
        if let Some(directory) = directory {
            request = request.header("x-opencode-directory", encode_directory(directory));
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(TransportError::from_request)?;
        read_answer(response)
            .await?
            .map_err(ProviderError::from_refusal)
    }

    /// The providers OpenCode lists for `directory` and has not disabled,
    /// each with its enabled models, in catalog order. A failed read is an
    /// error and never an empty list.
    pub async fn listed_providers(
        &self,
        directory: Option<&str>,
    ) -> Result<Vec<ListedProvider>, TransportError> {
        let (models, providers) = tokio::try_join!(
            self.get_json::<wire::Located<Vec<wire::ModelInfo>>>(
                self.opencode_url("model"),
                directory
            ),
            self.get_json::<wire::Located<Vec<wire::ProviderInfo>>>(
                self.opencode_url("provider"),
                directory
            ),
        )?;
        let models: Vec<_> = models
            .data
            .into_iter()
            .map(projection::model)
            .filter(|model| model.enabled)
            .collect();
        Ok(providers
            .data
            .into_iter()
            .map(projection::provider)
            .filter(|provider| provider.enabled)
            .map(|provider| ListedProvider {
                models: models
                    .iter()
                    .filter(|model| model.provider_id == provider.id)
                    .map(|model| ListedModel {
                        id: model.id.clone(),
                        name: model.name.clone(),
                        context_limit: model.context_limit,
                    })
                    .collect(),
                id: provider.id,
                name: provider.name,
            })
            .collect())
    }

    /// The integrations of `directory` (`GET /api/integration`). A failed
    /// read is an error. An entry this client cannot read is left out and
    /// the others stay complete.
    pub async fn integrations(
        &self,
        directory: Option<&str>,
    ) -> Result<Vec<Integration>, TransportError> {
        let listed: wire::Located<Vec<Value>> = self
            .get_json(self.opencode_url("integration"), directory)
            .await?;
        Ok(listed
            .data
            .into_iter()
            .filter_map(|entry| serde_json::from_value::<IntegrationWire>(entry).ok())
            .map(integration)
            .collect())
    }

    /// Stores `key` as a credential of `integration_id`
    /// (`POST /api/integration/:id/connect/key`).
    pub async fn connect_provider_key(
        &self,
        integration_id: &str,
        key: &SecretKey,
        directory: Option<&str>,
    ) -> Result<(), ProviderError> {
        self.provider_write(
            Method::POST,
            self.integration_url(integration_id, &["connect", "key"]),
            directory,
            Some(json!({ "key": key.expose() })),
        )
        .await
        .map(drop)
    }

    /// Connects through credentials managed outside OpenCode
    /// (`POST /api/integration/:id/connect/external`).
    pub async fn connect_provider_external(
        &self,
        integration_id: &str,
        method_id: &str,
        directory: Option<&str>,
    ) -> Result<(), ProviderError> {
        self.provider_write(
            Method::POST,
            self.integration_url(integration_id, &["connect", "external"]),
            directory,
            Some(json!({ "methodID": method_id })),
        )
        .await
        .map(drop)
    }

    /// Opens an OAuth attempt (`POST /api/integration/:id/connect/oauth`).
    pub async fn start_provider_oauth(
        &self,
        integration_id: &str,
        method_id: &str,
        directory: Option<&str>,
    ) -> Result<OAuthAttempt, ProviderError> {
        let bytes = self
            .provider_write(
                Method::POST,
                self.integration_url(integration_id, &["connect", "oauth"]),
                directory,
                Some(json!({ "methodID": method_id })),
            )
            .await?;
        let attempt: wire::Located<AttemptWire> = parse(&bytes)?;
        let attempt = attempt.data;
        if attempt.attempt_id.is_empty() {
            return Err(TransportError::Malformed("the attempt has no ID".to_owned()).into());
        }
        Ok(OAuthAttempt {
            id: attempt.attempt_id,
            mode: match attempt.mode.as_deref() {
                Some("code") => OAuthMode::Code,
                _ => OAuthMode::Auto,
            },
            url: attempt.url,
            instructions: attempt.instructions,
        })
    }

    /// Where an attempt stands
    /// (`GET /api/integration/:id/connect/oauth/:attempt`). A failed read is
    /// an error and never a pending attempt.
    pub async fn provider_oauth_status(
        &self,
        integration_id: &str,
        attempt_id: &str,
        directory: Option<&str>,
    ) -> Result<OAuthStatus, ProviderError> {
        let bytes = self
            .provider_write(
                Method::GET,
                self.integration_url(integration_id, &["connect", "oauth", attempt_id]),
                directory,
                None,
            )
            .await?;
        let status: wire::Located<AttemptStatusWire> = parse(&bytes)?;
        Ok(match status.data.status.as_str() {
            "pending" => OAuthStatus::Pending,
            "complete" => OAuthStatus::Complete,
            "expired" => OAuthStatus::Expired,
            "failed" => OAuthStatus::Failed {
                message: status.data.message.unwrap_or_default(),
            },
            other => {
                return Err(
                    TransportError::Malformed(format!("unknown attempt status {other}")).into(),
                );
            }
        })
    }

    /// Hands the code the user copied out of the browser to the attempt
    /// (`POST .../connect/oauth/:attempt/complete`).
    pub async fn complete_provider_oauth(
        &self,
        integration_id: &str,
        attempt_id: &str,
        code: &SecretKey,
        directory: Option<&str>,
    ) -> Result<(), ProviderError> {
        self.provider_write(
            Method::POST,
            self.integration_url(
                integration_id,
                &["connect", "oauth", attempt_id, "complete"],
            ),
            directory,
            Some(json!({ "code": code.expose() })),
        )
        .await
        .map(drop)
    }

    /// Drops an attempt the user walked away from
    /// (`DELETE .../connect/oauth/:attempt`).
    pub async fn cancel_provider_oauth(
        &self,
        integration_id: &str,
        attempt_id: &str,
        directory: Option<&str>,
    ) -> Result<(), ProviderError> {
        self.provider_write(
            Method::DELETE,
            self.integration_url(integration_id, &["connect", "oauth", attempt_id]),
            directory,
            None,
        )
        .await
        .map(drop)
    }

    /// Removes one stored credential (`DELETE /api/credential/:id`). The
    /// route is global and stays allowed in enterprise mode.
    pub async fn remove_provider_credential(
        &self,
        credential_id: &str,
    ) -> Result<(), ProviderError> {
        let mut url = self.opencode_url("credential/");
        url.path_segments_mut()
            .expect("http and https URLs have path segments")
            .pop_if_empty()
            .push(credential_id);
        self.provider_write(Method::DELETE, url, None, None)
            .await
            .map(drop)
    }

    /// Writes a custom provider into the user's OpenCode config
    /// (`PUT /api/provider`).
    pub async fn save_custom_provider(
        &self,
        provider: &CustomProvider,
        directory: Option<&str>,
    ) -> Result<(), ProviderError> {
        let mut url = self.endpoint().url("api/provider");
        if let Some(directory) = directory {
            url.query_pairs_mut().append_pair("directory", directory);
        }
        self.provider_write(Method::PUT, url, None, Some(provider.body()))
            .await
            .map(drop)
    }

    /// Replaces the models hidden from the pickers (`hiddenModels` of the
    /// settings document) and returns the list as the server merged it.
    pub async fn save_hidden_models(
        &self,
        hidden: &[(String, String)],
    ) -> Result<Vec<(String, String)>, ProviderError> {
        let entries: Vec<Value> = hidden
            .iter()
            .map(
                |(provider_id, model_id)| json!({ "providerID": provider_id, "modelID": model_id }),
            )
            .collect();
        let bytes = self
            .provider_write(
                Method::PUT,
                self.settings_url(),
                None,
                Some(json!({ "hiddenModels": entries })),
            )
            .await?;
        let document: Value = parse(&bytes)?;
        if !document.is_object() {
            return Err(TransportError::Malformed(
                "the settings answer is not an object".to_owned(),
            )
            .into());
        }
        Ok(model_refs(document.get("hiddenModels")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-fixture-never-shown";

    fn integrations_body() -> Value {
        json!({"location": {}, "data": [
            {
                "id": "anthropic",
                "name": "Anthropic",
                "methods": [
                    {"type": "key"},
                    {"type": "oauth", "id": "claude-pro", "label": "Claude Pro"},
                    {"type": "command", "id": "cli", "label": "CLI"},
                ],
                "connections": [
                    {"type": "credential", "id": "cred_1", "label": "Work", "method": "key"},
                    {"type": "env", "name": "ANTHROPIC_API_KEY",
                     "status": {"status": "needs_auth", "message": "expired"}},
                ],
            },
            {
                "id": "azure",
                "name": "",
                "methods": [
                    {"type": "key", "form": [
                        {"type": "string", "key": "resource", "required": true},
                    ]},
                    {"type": "external", "id": "cli", "label": "Azure CLI", "form": [
                        {"type": "external", "key": "docs", "required": true},
                        {"type": "string", "key": "tenant"},
                    ]},
                ],
                "connections": [],
            },
            {"id": "exa", "name": "Exa", "metadata": {"websearch": true},
             "methods": [{"type": "key"}], "connections": []},
            {"name": "an entry without an ID"},
        ]})
    }

    #[tokio::test]
    async fn integrations_project_methods_and_connections_and_skip_a_broken_entry() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/api/integration") => (200, integrations_body()),
                _ => (500, json!({"error": "unexpected"})),
            }
        }))
        .await;
        let integrations = client.integrations(Some("/repo")).await.expect("list");
        assert_eq!(integrations.len(), 3, "the entry without an ID is left out");
        let anthropic = &integrations[0];
        assert!(anthropic.accepts_key && !anthropic.key_needs_answers);
        assert_eq!(
            anthropic.sign_in,
            [SignInMethod {
                id: "claude-pro".into(),
                label: Some("Claude Pro".into()),
                kind: SignInKind::OAuth,
                needs_answers: false,
            }],
            "a command method is not a sign-in button"
        );
        assert_eq!(anthropic.credentials().count(), 1);
        assert!(anthropic.connections[1].needs_auth());
        let azure = &integrations[1];
        assert_eq!(azure.name, "azure", "an empty name falls back to the ID");
        assert!(azure.key_needs_answers);
        assert!(
            !azure.sign_in[0].needs_answers,
            "an external link field and an optional field need no answer"
        );
        assert!(integrations[2].web_search);
        assert_eq!(recorded.lock().expect("log").len(), 1);
    }

    #[tokio::test]
    async fn a_failed_list_is_an_error_and_never_an_empty_list() {
        let (client, _) =
            crate::test_server::serve(Box::new(|_| (500, json!({"error": "down"})))).await;
        assert!(client.integrations(None).await.is_err());
        assert!(client.listed_providers(None).await.is_err());
    }

    #[tokio::test]
    async fn providers_list_enabled_models_under_enabled_providers() {
        let (client, _) =
            crate::test_server::serve(Box::new(|request| match request.path.as_str() {
                "/api/provider" => (
                    200,
                    json!({"location": {}, "data": [
                        {"id": "anthropic", "name": "Anthropic"},
                        {"id": "off", "name": "Off", "activation": "disabled"},
                        {"id": "bare", "name": "Bare"},
                    ]}),
                ),
                "/api/model" => (
                    200,
                    json!({"location": {}, "data": [
                        {"id": "sonnet", "modelID": "sonnet", "providerID": "anthropic",
                         "name": "Sonnet", "enabled": true, "limit": {"context": 200000}},
                        {"id": "old", "modelID": "old", "providerID": "anthropic",
                         "enabled": false},
                        {"id": "m", "modelID": "m", "providerID": "off", "enabled": true},
                    ]}),
                ),
                _ => (500, json!({"error": "unexpected"})),
            }))
            .await;
        let providers = client.listed_providers(Some("/repo")).await.expect("list");
        assert_eq!(
            providers,
            [
                ListedProvider {
                    id: "anthropic".into(),
                    name: Some("Anthropic".into()),
                    models: vec![ListedModel {
                        id: "sonnet".into(),
                        name: Some("Sonnet".into()),
                        context_limit: Some(200_000),
                    }],
                },
                ListedProvider {
                    id: "bare".into(),
                    name: Some("Bare".into()),
                    models: Vec::new(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_key_reaches_only_the_connect_route_and_no_error_carries_it() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            match (request.method.as_str(), request.path.as_str()) {
                ("POST", "/api/integration/anthropic/connect/key") => (200, json!({})),
                // A server that echoes the request back in its refusal.
                ("POST", "/api/integration/echo/connect/key") => (
                    400,
                    json!({"error": format!("bad key {}", request.body["key"])}),
                ),
                ("POST", "/api/integration/locked/connect/key") => (
                    403,
                    json!({"error": "Not available in enterprise mode", "code": "enterprise_mode"}),
                ),
                ("POST", "/api/integration/new/connect/key") => {
                    (404, json!({"error": "Integration not found"}))
                }
                _ => (500, json!({"error": "unexpected"})),
            }
        }))
        .await;
        let key = SecretKey::new(&format!("  {KEY} ")).expect("a key");
        assert_eq!(format!("{key:?}"), "SecretKey(..)");
        assert!(SecretKey::new("   ").is_none());
        client
            .connect_provider_key("anthropic", &key, Some("/repo"))
            .await
            .expect("connect");
        let refused = client
            .connect_provider_key("echo", &key, None)
            .await
            .expect_err("refused");
        assert!(
            !refused.to_string().contains(KEY) && !format!("{refused}").contains(KEY),
            "the displayed error names the status alone"
        );
        assert!(matches!(
            client.connect_provider_key("locked", &key, None).await,
            Err(ProviderError::EnterpriseMode)
        ));
        let missing = client
            .connect_provider_key("new", &key, None)
            .await
            .expect_err("not registered yet");
        assert!(missing.is_not_found());
        let recorded = recorded.lock().expect("log");
        assert_eq!(
            recorded[0].body,
            json!({ "key": KEY }),
            "the trimmed key and nothing else"
        );
    }

    #[tokio::test]
    async fn an_oauth_attempt_starts_polls_completes_and_cancels() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            match (request.method.as_str(), request.path.as_str()) {
                ("POST", "/api/integration/openai/connect/oauth") => (
                    200,
                    json!({"location": {}, "data": {
                        "attemptID": "att_1",
                        "url": "https://auth.example.test/device",
                        "instructions": "Enter code: ABCD-1234",
                        "mode": "auto",
                        "time": {"created": 1, "expires": 2},
                    }}),
                ),
                ("POST", "/api/integration/coded/connect/oauth") => (
                    200,
                    json!({"location": {}, "data": {"attemptID": "att_2", "mode": "code"}}),
                ),
                ("GET", "/api/integration/openai/connect/oauth/att_1") => (
                    200,
                    json!({"location": {}, "data": {"status": "failed", "message": "declined"}}),
                ),
                ("GET", _) => (500, json!({"error": "down"})),
                ("POST", "/api/integration/coded/connect/oauth/att_2/complete") => (200, json!({})),
                ("DELETE", "/api/integration/openai/connect/oauth/att_1") => (200, json!({})),
                _ => (500, json!({"error": "unexpected"})),
            }
        }))
        .await;
        let attempt = client
            .start_provider_oauth("openai", "chatgpt", Some("/repo"))
            .await
            .expect("start");
        assert_eq!(attempt.mode, OAuthMode::Auto);
        assert_eq!(attempt.url, "https://auth.example.test/device");
        let coded = client
            .start_provider_oauth("coded", "browser", None)
            .await
            .expect("start");
        assert_eq!(coded.mode, OAuthMode::Code);
        assert_eq!(
            client
                .provider_oauth_status("openai", "att_1", None)
                .await
                .expect("status"),
            OAuthStatus::Failed {
                message: "declined".into()
            }
        );
        assert!(
            client
                .provider_oauth_status("coded", "att_2", None)
                .await
                .is_err(),
            "a failed status read is never a pending attempt"
        );
        let code = SecretKey::new("code-123").expect("a code");
        client
            .complete_provider_oauth("coded", "att_2", &code, None)
            .await
            .expect("complete");
        client
            .cancel_provider_oauth("openai", "att_1", None)
            .await
            .expect("cancel");
        let recorded = recorded.lock().expect("log");
        assert_eq!(recorded[0].body, json!({"methodID": "chatgpt"}));
        assert_eq!(recorded[4].body, json!({"code": "code-123"}));
    }

    #[tokio::test]
    async fn a_custom_provider_is_written_without_a_key_and_a_credential_is_removed() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            match (request.method.as_str(), request.path.as_str()) {
                ("PUT", "/api/provider") => (200, json!({"success": true})),
                ("DELETE", "/api/credential/cred%201") | ("DELETE", "/api/credential/cred 1") => {
                    (200, json!({}))
                }
                ("PUT", "/api/config/settings") => (
                    200,
                    json!({"hiddenModels": [
                        {"providerID": " anthropic ", "modelID": "sonnet"},
                        {"providerID": "anthropic", "modelID": "sonnet"},
                        {"providerID": "", "modelID": "x"},
                    ]}),
                ),
                _ => (403, json!({"error": "refused", "code": "enterprise_mode"})),
            }
        }))
        .await;
        let provider = CustomProvider {
            id: "local".into(),
            name: "Local".into(),
            protocol: CustomProtocol::OpenAiChat,
            base_url: "http://localhost:11434/v1".into(),
            env: Some("LOCAL_KEY".into()),
            models: vec![("llama".into(), "Llama".into())],
            has_credential: false,
        };
        client
            .save_custom_provider(&provider, Some("/repo"))
            .await
            .expect("save");
        client
            .remove_provider_credential("cred 1")
            .await
            .expect("remove");
        let hidden = client
            .save_hidden_models(&[("anthropic".into(), "sonnet".into())])
            .await
            .expect("hidden");
        assert_eq!(hidden, [("anthropic".to_owned(), "sonnet".to_owned())]);
        let recorded = recorded.lock().expect("log");
        assert_eq!(
            recorded[0].body,
            json!({
                "providerID": "local",
                "config": {
                    "package": "aisdk:@ai-sdk/openai-compatible",
                    "name": "Local",
                    "settings": {"baseURL": "http://localhost:11434/v1"},
                    "models": {"llama": {"modelID": "llama", "name": "Llama"}},
                    "env": ["LOCAL_KEY"],
                },
                "scope": "user",
                "hasCredential": false,
            })
        );
        assert_eq!(
            recorded[2].body,
            json!({"hiddenModels": [{"providerID": "anthropic", "modelID": "sonnet"}]})
        );
    }
}
