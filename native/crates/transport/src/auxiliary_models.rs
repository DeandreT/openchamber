//! Existing agent configuration for summary/title purposes, independent of send picks.
use openchamber_native_core::model_identifier::{format_model_selection, parse_model_selection};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    client::{ServerClient, encode_directory, read_json},
    error::TransportError,
    files::read_answer,
    wire,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Purpose {
    Compaction,
    Title,
}
impl Purpose {
    pub fn agent(self) -> &'static str {
        match self {
            Self::Compaction => "compaction",
            Self::Title => "title",
        }
    }
    pub fn index(self) -> usize {
        match self {
            Self::Compaction => 0,
            Self::Title => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PurposeModel {
    pub model: Option<String>,
    pub source: String,
    pub scope: Option<String>,
    pub path: Option<String>,
    /// Opaque source/model precondition from the existing configuration endpoint.
    pub revision: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    source: String,
    scope: Option<String>,
    path: Option<String>,
    config: Value,
    model_revision: Option<String>,
}
impl TryFrom<Stored> for PurposeModel {
    type Error = TransportError;
    fn try_from(raw: Stored) -> Result<Self, Self::Error> {
        if !["md", "json", "none"].contains(&raw.source.as_str())
            || !raw.config.is_object()
            || (raw.source == "none" && (raw.scope.is_some() || raw.path.is_some()))
            || (raw.source != "none"
                && (raw.path.is_none()
                    || !matches!(raw.scope.as_deref(), Some("user" | "project"))))
        {
            return Err(TransportError::Malformed(
                "agent configuration source".into(),
            ));
        }
        let model = match raw.config.get("model") {
            None => None,
            Some(Value::String(value)) => Some(
                parse_model_selection(value, None)
                    .and_then(|value| format_model_selection(&value))
                    .ok_or_else(|| TransportError::Malformed("agent model reference".into()))?,
            ),
            _ => return Err(TransportError::Malformed("agent model reference".into())),
        };
        if raw.model_revision.as_ref().is_some_and(|revision| {
            revision.len() != 64 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(TransportError::Malformed("agent model revision".into()));
        }
        Ok(Self {
            model,
            source: raw.source,
            scope: raw.scope,
            path: raw.path,
            revision: raw.model_revision,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextModel {
    pub value: String,
    pub label: String,
}

fn ordinary_package(package: &str) -> bool {
    package
        .strip_prefix("@opencode/ai/providers/")
        .is_some_and(|name| {
            let (name, route) = name.split_once('/').unwrap_or((name, ""));
            // Ordinary provider exports in the pinned @opencode/ai 2.2.7 package.
            [
                "alibaba",
                "amazon-bedrock",
                "anthropic",
                "anthropic-compatible",
                "azure",
                "baseten",
                "cerebras",
                "cloudflare-ai-gateway",
                "cloudflare-workers-ai",
                "cohere",
                "deepinfra",
                "deepseek",
                "digitalocean",
                "fireworks",
                "google",
                "google-vertex",
                "google-vertex-messages",
                "groq",
                "meta",
                "minimax",
                "mistral",
                "moonshot",
                "openai",
                "openai-compatible",
                "openai-compatible-responses",
                "opencode",
                "openrouter",
                "togetherai",
                "typesafe-ai",
                "venice",
                "vercel-ai-gateway",
                "xai",
                "zai",
                "zai-coding-plan",
            ]
            .contains(&name)
                && [
                    "",
                    "chat",
                    "messages",
                    "responses",
                    "mantle/chat",
                    "mantle/messages",
                    "mantle/responses",
                ]
                .contains(&route)
        })
        || package.strip_prefix("aisdk:@ai-sdk/").is_some_and(|name| {
            [
                "openai",
                "openai-compatible",
                "anthropic",
                "google",
                "google-vertex",
                "amazon-bedrock",
                "azure",
                "mistral",
                "cohere",
                "groq",
                "xai",
                "deepinfra",
                "cerebras",
                "togetherai",
                "alibaba",
            ]
            .contains(&name.split('/').next().unwrap_or_default())
        })
}

fn text_choices(
    models: Vec<Value>,
    providers: Vec<Value>,
) -> Result<Vec<TextModel>, TransportError> {
    if models.is_empty() || providers.is_empty() {
        return Err(TransportError::Malformed("model catalog not ready".into()));
    }
    if providers.iter().any(|provider| {
        provider["id"].as_str().is_none_or(str::is_empty) || !provider["package"].is_string()
    }) || models.iter().any(|model| {
        model["id"].as_str().is_none_or(str::is_empty)
            || model["providerID"].as_str().is_none_or(str::is_empty)
            || !model["enabled"].is_boolean()
            || !model["capabilities"]["output"]
                .as_array()
                .is_some_and(|output| output.iter().all(Value::is_string))
    }) {
        return Err(TransportError::Malformed("model catalog entries".into()));
    }
    let mut choices = Vec::new();
    for model in models {
        let Some(provider_id) = model["providerID"].as_str() else {
            return Err(TransportError::Malformed("model provider".into()));
        };
        let Some(provider) = providers
            .iter()
            .find(|p| p["id"].as_str() == Some(provider_id))
        else {
            continue;
        };
        if model["enabled"] != true
            || model["type"].as_str().unwrap_or("chat") != "chat"
            || provider["activation"] == "disabled"
            || !provider
                .get("package")
                .and_then(Value::as_str)
                .is_some_and(ordinary_package)
            || provider
                .pointer("/settings/openchamber/nativeAgent")
                .is_some()
            || !model["capabilities"]["output"]
                .as_array()
                .is_some_and(|output| output.iter().any(|v| v == "text"))
            || !model
                .get("package")
                .or_else(|| provider.get("package"))
                .and_then(Value::as_str)
                .is_some_and(ordinary_package)
        {
            continue;
        }
        let id = model["id"]
            .as_str()
            .ok_or_else(|| TransportError::Malformed("model identity".into()))?;
        let value = format!("{provider_id}/{id}");
        let label = format!(
            "{} / {}",
            provider["name"].as_str().unwrap_or(provider_id),
            model["name"].as_str().unwrap_or(id)
        );
        choices.push(TextModel {
            value: value.clone(),
            label: label.clone(),
        });
        if let Some(variants) = model["variants"].as_array() {
            for variant in variants {
                if let Some(id) = variant["id"].as_str() {
                    choices.push(TextModel {
                        value: format!("{value}#{id}"),
                        label: format!("{label} / {id}"),
                    });
                }
            }
        }
    }
    Ok(choices)
}

impl ServerClient {
    fn purpose_request(
        &self,
        directory: &str,
        purpose: Purpose,
        method: Method,
    ) -> crate::request::RequestBuilder {
        let suffix = if method == Method::GET { "/config" } else { "" };
        self.authorized(
            method,
            self.endpoint()
                .url(&format!("api/config/agents/{}{suffix}", purpose.agent())),
        )
        .header("x-opencode-directory", encode_directory(directory))
        .header("x-opencode-directory-encoding", "uri")
    }

    pub async fn purpose_model(
        &self,
        directory: &str,
        purpose: Purpose,
    ) -> Result<PurposeModel, TransportError> {
        let response = self
            .purpose_request(directory, purpose, Method::GET)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        read_json::<Stored>(response).await?.try_into()
    }

    pub async fn purpose_text_models(
        &self,
        directory: &str,
    ) -> Result<Vec<TextModel>, TransportError> {
        let (models, providers) = tokio::try_join!(
            self.get_json::<wire::Located<Vec<Value>>>(self.opencode_url("model"), Some(directory)),
            self.get_json::<wire::Located<Vec<Value>>>(
                self.opencode_url("provider"),
                Some(directory)
            )
        )?;
        text_choices(models.data, providers.data)
    }

    /// A failed write never becomes a successful empty selection or an automatic retry.
    pub async fn save_purpose_model(
        &self,
        directory: &str,
        purpose: Purpose,
        expected: &PurposeModel,
        model: Option<&str>,
    ) -> Result<PurposeModel, TransportError> {
        let current = self.purpose_model(directory, purpose).await?;
        if current.source != expected.source
            || current.scope != expected.scope
            || current.path != expected.path
        {
            return Err(TransportError::Http { status: 409 });
        }
        if current.model.as_deref() == model {
            return Ok(current);
        }
        if current.model != expected.model || current.revision != expected.revision {
            return Err(TransportError::Http { status: 409 });
        }
        let revision = expected.revision.as_deref().ok_or_else(|| {
            TransportError::Malformed("agent source preconditions unavailable".into())
        })?;
        let response = self
            .purpose_request(directory, purpose, Method::PATCH)
            .header("If-Match", format!("\"{revision}\""))
            .json(&json!({"model": model}))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        if let Err(refusal) = read_answer(response).await? {
            return Err(TransportError::Http {
                status: refusal.status.as_u16(),
            });
        }
        self.purpose_model(directory, purpose).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_values_do_not_depend_on_catalog_availability() {
        let raw: Stored = serde_json::from_value(json!({"source":"json","scope":"project","path":"/work/opencode.jsonc","config":{"model":"devin/local#saved"}})).expect("stored model");
        let saved = PurposeModel::try_from(raw).expect("purpose model");
        assert_eq!(saved.model.as_deref(), Some("devin/local#saved"));
        assert!(text_choices(vec![], vec![]).is_err());
        assert_eq!(saved.scope.as_deref(), Some("project"));
    }
    #[test]
    fn native_packages_and_markers_are_not_ordinary_text_choices() {
        let model =
            json!({"providerID":"p","id":"text","enabled":true,"capabilities":{"output":["text"]}});
        let provider = json!({"id":"p","package":"@opencode/ai/providers/openai"});
        assert_eq!(
            text_choices(vec![model.clone()], vec![provider.clone()])
                .expect("text choices")
                .len(),
            1
        );
        let mut native = provider.clone();
        native["settings"] = json!({"openchamber":{"nativeAgent":{"protocolVersion":1}}});
        assert!(
            text_choices(vec![model.clone()], vec![native])
                .expect("text choices")
                .is_empty()
        );
        let mut native = provider;
        native["package"] = json!("aisdk:openchamber-devin");
        assert!(
            text_choices(vec![model], vec![native])
                .expect("text choices")
                .is_empty()
        );
    }

    fn stored(model: Option<&str>) -> Value {
        let mut config = json!({});
        if let Some(model) = model {
            config["model"] = json!(model);
        }
        json!({"source":"json","scope":"project","path":"/work/opencode.jsonc","config":config,"modelRevision":"a".repeat(64)})
    }

    #[tokio::test]
    async fn purpose_requests_capture_bearer_prefix_and_marked_actual_directory() {
        let client = ServerClient::new(
            crate::endpoint::Endpoint::parse("https://example.test/prefix/").expect("endpoint"),
            crate::credential::Credential::ClientToken("oc_client_synthetic".into()),
        )
        .expect("client");
        for method in [Method::GET, Method::PATCH] {
            let request = client
                .purpose_request(
                    "C:/work/percent% space/日本語",
                    Purpose::Title,
                    method.clone(),
                )
                .build()
                .expect("request");
            assert_eq!(
                request.url().path(),
                if method == Method::GET {
                    "/prefix/api/config/agents/title/config"
                } else {
                    "/prefix/api/config/agents/title"
                }
            );
            assert_eq!(
                request.headers()["authorization"],
                "Bearer oc_client_synthetic"
            );
            assert_eq!(
                request.headers()["x-opencode-directory"],
                encode_directory("C:/work/percent% space/日本語")
            );
            assert_eq!(request.headers()["x-opencode-directory-encoding"], "uri");
        }
    }

    #[tokio::test]
    async fn failed_save_retains_expected_value_and_retries_only_explicitly() {
        use std::sync::{Arc, Mutex};
        let count = Arc::new(Mutex::new(0));
        let writes = count.clone();
        let (client, requests) = crate::test_server::serve(Box::new(move |request| {
            if request.method == "PATCH" {
                *writes.lock().expect("write count") += 1;
                return (500, json!({"error":"fixture refusal"}));
            }
            (200, stored(Some("ordinary/old")))
        }))
        .await;
        let expected = client
            .purpose_model("/work", Purpose::Compaction)
            .await
            .expect("purpose model");
        for _ in 0..2 {
            assert!(
                client
                    .save_purpose_model(
                        "/work",
                        Purpose::Compaction,
                        &expected,
                        Some("ordinary/new")
                    )
                    .await
                    .is_err()
            );
            assert_eq!(expected.model.as_deref(), Some("ordinary/old"));
        }
        assert_eq!(*count.lock().expect("write count"), 2);
        assert_eq!(
            requests
                .lock()
                .expect("requests")
                .iter()
                .filter(|request| request.method == "PATCH")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn changed_source_is_a_conflict_without_patch() {
        let (client, requests) = crate::test_server::serve(Box::new(|_| {
            let mut changed = stored(Some("ordinary/old"));
            changed["path"] = json!("/work/another.jsonc");
            (200, changed)
        }))
        .await;
        let expected = PurposeModel::try_from(
            serde_json::from_value::<Stored>(stored(Some("ordinary/old"))).expect("stored model"),
        )
        .expect("purpose model");
        assert!(matches!(
            client
                .save_purpose_model("/work", Purpose::Title, &expected, None)
                .await,
            Err(TransportError::Http { status: 409 })
        ));
        assert!(
            requests
                .lock()
                .expect("requests")
                .iter()
                .all(|request| request.method == "GET")
        );
    }

    #[tokio::test]
    async fn null_removal_reads_back_source_and_writes_only_model() {
        use std::sync::{Arc, Mutex};
        let saved = Arc::new(Mutex::new(Some("ordinary/old".to_string())));
        let held = saved.clone();
        let (client, requests) = crate::test_server::serve(Box::new(move |request| {
            if request.method == "PATCH" {
                assert_eq!(request.body, json!({"model":null}));
                *held.lock().expect("saved model") = None;
                (200, json!({"success":true}))
            } else {
                (200, stored(held.lock().expect("saved model").as_deref()))
            }
        }))
        .await;
        let expected = client
            .purpose_model("/work", Purpose::Title)
            .await
            .expect("purpose model");
        let observed = client
            .save_purpose_model("/work", Purpose::Title, &expected, None)
            .await
            .expect("save purpose model");
        assert_eq!(observed.model, None);
        assert_eq!(observed.path, expected.path);
        assert_eq!(
            requests
                .lock()
                .expect("requests")
                .iter()
                .filter(|request| request.method == "PATCH")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn catalog_envelopes_are_required_and_failed_reads_never_clear_saved_models() {
        let (client, _) = crate::test_server::serve(Box::new(|request| {
            match request.path.as_str() {
                "/api/model" => (200, json!({"data":[{"providerID":"p","id":"text","enabled":true,"capabilities":{"output":["text"]},"variants":[{"id":"high"}]}]})),
                "/api/provider" => (200, json!({"data":[{"id":"p","package":"@opencode/ai/providers/openai"}]})),
                _ => (200, stored(Some("unavailable/saved"))),
            }
        })).await;
        let saved = client
            .purpose_model("/work", Purpose::Title)
            .await
            .expect("purpose model");
        let choices = client
            .purpose_text_models("/work")
            .await
            .expect("text models");
        assert_eq!(
            choices
                .iter()
                .map(|entry| entry.value.as_str())
                .collect::<Vec<_>>(),
            ["p/text", "p/text#high"]
        );
        assert_eq!(saved.model.as_deref(), Some("unavailable/saved"));
        let (failed, _) =
            crate::test_server::serve(Box::new(|_| (503, json!({"error":"not ready"})))).await;
        assert!(failed.purpose_text_models("/work").await.is_err());
        assert_eq!(saved.model.as_deref(), Some("unavailable/saved"));
    }
}
