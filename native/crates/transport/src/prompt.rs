//! Text submission and interruption of a named session through OpenChamber.

use std::fmt;

use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::send_config::ModelChoice;
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};

use crate::client::ServerClient;
use crate::error::TransportError;

/// How OpenCode admits a prompt while a turn is running.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// Make the input available to the active turn at a steering boundary.
    Steer,
    /// Wait for an idle boundary before starting the input.
    Queue,
}

/// A text-only turn. Attachments, commands and synthetic context have separate
/// protocols and cannot be supplied through this API.
#[derive(Clone)]
pub struct TextPrompt {
    /// Stable client-generated ID retained across reconciliation.
    pub message_id: MessageId,
    /// Original text before server-side preparation hooks.
    pub text: String,
    /// When present, persists this selection before admitting the prompt.
    pub model: Option<ModelChoice>,
    /// When present, persists the agent after selecting the model.
    pub agent: Option<String>,
    /// `None` leaves the delivery policy to OpenCode.
    pub delivery: Option<Delivery>,
}

impl fmt::Debug for TextPrompt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TextPrompt")
            .field("message_id", &self.message_id)
            .field("text", &"[redacted]")
            .field("model", &self.model)
            .field("agent", &self.agent)
            .field("delivery", &self.delivery)
            .finish()
    }
}

/// Admission acknowledges a matching user item. Prepared content comes from
/// history and events; this receipt does not attest an exact text echo or turn completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptReceipt {
    /// The session that admitted the item.
    pub session_id: SessionId,
    /// The admitted item, which may still be waiting in the inbox.
    pub message_id: MessageId,
    /// Server admission time in milliseconds since the Unix epoch.
    pub created: u64,
    /// Effective delivery after server-side preparation.
    pub delivery: Delivery,
}

/// Which part of submission failed. Earlier selection changes can persist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendStage {
    /// Local validation failed before any request.
    Validation,
    /// The model-selection request failed.
    Model,
    /// The agent-selection request failed.
    Agent,
    /// The admission request or its response failed.
    Prompt,
}

/// Whether the client has evidence that the prompt was admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptAdmission {
    /// No admission request was made; earlier selections may have persisted.
    NotAttempted,
    /// The admission request received a definite rejection.
    Rejected,
    /// Reconcile this exact message ID from inbox/events/history before any resend.
    Unknown,
}

/// Keeps the correlation IDs even when the response was lost. The caller keeps
/// its draft; this transport never retries mutations or rolls selections back.
#[derive(Debug, thiserror::Error)]
#[error("prompt submission failed at {stage:?}: {source}")]
pub struct SendError {
    pub session_id: SessionId,
    pub message_id: MessageId,
    pub stage: SendStage,
    pub admission: PromptAdmission,
    #[source]
    pub source: TransportError,
}

/// An interruption failure. A lost response can leave interruption applied.
#[derive(Debug, thiserror::Error)]
#[error("session interruption failed: {source}")]
pub struct AbortError {
    pub session_id: SessionId,
    pub outcome_unknown: bool,
    #[source]
    pub source: TransportError,
}

#[derive(Serialize)]
struct PromptBody<'a> {
    id: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    delivery: Option<Delivery>,
}

#[derive(Serialize)]
struct ModelBody<'a> {
    model: ModelRef<'a>,
}

#[derive(Serialize)]
struct ModelRef<'a> {
    #[serde(rename = "providerID")]
    provider_id: &'a str,
    #[serde(rename = "id")]
    model_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    variant: Option<&'a str>,
}

#[derive(Serialize)]
struct AgentBody<'a> {
    agent: &'a str,
}

#[derive(Deserialize)]
struct PromptResponse {
    data: InboxUser,
}

#[derive(Deserialize)]
struct InboxUser {
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(rename = "type")]
    kind: String,
    time: Created,
    // Preparation hooks can transform payload and add attachments. Admission
    // consumes only identity, time and delivery; history/events own content.
    delivery: Delivery,
}

#[derive(Deserialize)]
struct Created {
    created: u64,
}

#[derive(Deserialize)]
struct InterruptResponse {
    interrupted: bool,
}

impl ServerClient {
    pub(crate) fn mutation_request(
        &self,
        session_id: &SessionId,
        operation: &str,
    ) -> RequestBuilder {
        let mut url = self.endpoint().url("api/session/");
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .pop_if_empty()
            .extend([session_id.as_str(), operation]);
        self.authorized(Method::POST, url)
    }

    pub(crate) fn prompt_request(
        &self,
        session: &SessionId,
        prompt: &TextPrompt,
    ) -> RequestBuilder {
        self.mutation_request(session, "prompt").json(&PromptBody {
            id: prompt.message_id.as_str(),
            text: &prompt.text,
            delivery: prompt.delivery,
        })
    }

    pub(crate) fn model_request(&self, session: &SessionId, model: &ModelChoice) -> RequestBuilder {
        self.mutation_request(session, "model").json(&ModelBody {
            model: ModelRef {
                provider_id: &model.provider_id,
                model_id: &model.model_id,
                variant: model
                    .variant
                    .as_deref()
                    .filter(|variant| !variant.is_empty()),
            },
        })
    }

    pub(crate) fn agent_request(&self, session: &SessionId, agent: &str) -> RequestBuilder {
        self.mutation_request(session, "agent")
            .json(&AgentBody { agent })
    }

    /// Selects model, then agent, then admits the exact client-generated ID.
    /// Dropping the future cancels local I/O only. Once polled, cancellation
    /// requires reconciliation just like an unknown response; it cannot undo
    /// an admitted item or a completed selection.
    pub async fn send_text(
        &self,
        session: &SessionId,
        prompt: &TextPrompt,
    ) -> Result<PromptReceipt, SendError> {
        let failure = |stage, source| SendError {
            session_id: session.clone(),
            message_id: prompt.message_id.clone(),
            stage,
            admission: if stage != SendStage::Prompt {
                PromptAdmission::NotAttempted
            } else if definitely_rejected(&source) {
                PromptAdmission::Rejected
            } else {
                PromptAdmission::Unknown
            },
            source,
        };
        if session.as_str().is_empty()
            || !prompt.message_id.as_str().starts_with("msg_")
            || prompt.text.trim().is_empty()
            || prompt
                .agent
                .as_deref()
                .is_some_and(|agent| agent.trim().is_empty())
            || prompt.model.as_ref().is_some_and(|model| {
                model.provider_id.trim().is_empty() || model.model_id.trim().is_empty()
            })
        {
            return Err(failure(
                SendStage::Validation,
                malformed("invalid text prompt"),
            ));
        }
        if let Some(model) = &prompt.model {
            send_checked(self.model_request(session, model), StatusCode::NO_CONTENT)
                .await
                .map_err(|error| failure(SendStage::Model, error))?;
        }
        if let Some(agent) = &prompt.agent {
            send_checked(self.agent_request(session, agent), StatusCode::NO_CONTENT)
                .await
                .map_err(|error| failure(SendStage::Agent, error))?;
        }
        let response = send_checked(self.prompt_request(session, prompt), StatusCode::OK)
            .await
            .map_err(|error| failure(SendStage::Prompt, error))?;
        let reply: PromptResponse = crate::client::read_json(response)
            .await
            .map_err(|error| failure(SendStage::Prompt, receipt_error(error)))?;
        let item = reply.data;
        if item.id != prompt.message_id.as_str()
            || item.session_id != session.as_str()
            || item.kind != "user"
        {
            return Err(failure(
                SendStage::Prompt,
                malformed("prompt receipt does not match the submitted turn"),
            ));
        }
        Ok(PromptReceipt {
            session_id: session.clone(),
            message_id: prompt.message_id.clone(),
            created: item.time.created,
            delivery: item.delivery,
        })
    }

    /// Interrupts only this named session. `false` means no turn was running.
    /// Neither cancellation nor a failed answer triggers another mutation.
    pub async fn abort_session(&self, session: &SessionId) -> Result<bool, AbortError> {
        let failure = |source| AbortError {
            session_id: session.clone(),
            outcome_unknown: !definitely_rejected(&source),
            source,
        };
        if session.as_str().is_empty() {
            return Err(AbortError {
                session_id: session.clone(),
                outcome_unknown: false,
                source: malformed("a session ID is required"),
            });
        }
        let response = send_checked(self.mutation_request(session, "interrupt"), StatusCode::OK)
            .await
            .map_err(failure)?;
        let reply: InterruptResponse = crate::client::read_json(response)
            .await
            .map_err(|error| failure(receipt_error(error)))?;
        Ok(reply.interrupted)
    }
}

fn malformed(message: &str) -> TransportError {
    TransportError::Malformed(message.to_owned())
}

fn receipt_error(error: TransportError) -> TransportError {
    match error {
        // Serde errors can quote rejected payload fields. Keep content out of diagnostics.
        TransportError::Malformed(_) => malformed("unrecognized mutation receipt"),
        other => other,
    }
}

fn definitely_rejected(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::AuthenticationRequired
            | TransportError::Http {
                status: 400 | 401 | 403 | 404
            }
    )
}

async fn send_checked(
    request: RequestBuilder,
    expected: StatusCode,
) -> Result<Response, TransportError> {
    let response = request.send().await.map_err(TransportError::from_request)?;
    let status = response.status();
    if status.is_redirection() {
        return Err(TransportError::Redirected);
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(TransportError::AuthenticationRequired);
    }
    if status != expected {
        return Err(TransportError::Http {
            status: status.as_u16(),
        });
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use crate::credential::Credential;
    use crate::endpoint::Endpoint;

    use super::*;

    #[test]
    fn prompt_debug_never_contains_the_text() {
        let prompt = TextPrompt {
            message_id: MessageId::new("msg_test"),
            text: "private prompt content".to_owned(),
            model: None,
            agent: None,
            delivery: None,
        };
        let debug = format!("{prompt:?}");
        assert!(!debug.contains(&prompt.text));
        assert!(debug.contains("[redacted]"));
    }

    #[test]
    fn mutation_requests_match_the_pinned_client() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/transport.json"))
            .expect("generated fixture");
        let client = ServerClient::new(
            Endpoint::parse("http://127.0.0.1:4799").expect("endpoint"),
            Credential::None,
        )
        .expect("client");
        let session = SessionId::new("ses_e9b49c443ffefXcWWPhCOga6nH");
        let model = ModelChoice {
            provider_id: "fixture".to_owned(),
            model_id: "text-model".to_owned(),
            variant: Some("high".to_owned()),
        };
        let prompt = TextPrompt {
            message_id: MessageId::new("msg_native_fixture_exact"),
            text: "Hello\n世界 ".to_owned(),
            model: None,
            agent: None,
            delivery: Some(Delivery::Queue),
        };
        let requests = [
            (
                "select prompt model",
                client.model_request(&session, &model),
            ),
            (
                "clear prompt variant",
                client.model_request(
                    &session,
                    &ModelChoice {
                        variant: None,
                        ..model
                    },
                ),
            ),
            (
                "select prompt agent",
                client.agent_request(&session, "build"),
            ),
            (
                "submit text prompt",
                client.prompt_request(&session, &prompt),
            ),
            (
                "submit default delivery",
                client.prompt_request(
                    &session,
                    &TextPrompt {
                        delivery: None,
                        ..prompt
                    },
                ),
            ),
            (
                "interrupt named session",
                client.mutation_request(&session, "interrupt"),
            ),
            (
                "interrupt idle session",
                client.mutation_request(&session, "interrupt"),
            ),
        ];
        for (name, builder) in requests {
            let expected = fixture["requests"]
                .as_array()
                .expect("requests")
                .iter()
                .find(|entry| entry["name"] == name)
                .expect("recorded operation");
            let actual = builder.build().expect("request");
            assert_eq!(actual.method().as_str(), expected["method"], "{name}");
            assert_eq!(actual.url().path(), expected["path"], "{name}");
            assert!(actual.url().query().is_none(), "{name} has no query");
            for (header, field) in [
                ("x-opencode-directory", "directoryHeader"),
                ("x-openchamber-part-shells", "partShellsHeader"),
                ("content-type", "contentType"),
            ] {
                assert_eq!(
                    actual
                        .headers()
                        .get(header)
                        .map(|value| value.to_str().expect("header")),
                    expected[field].as_str(),
                    "{name}: {header}"
                );
            }
            let actual_body: Option<Value> = actual
                .body()
                .and_then(reqwest::Body::as_bytes)
                .map(|bytes| serde_json::from_slice(bytes).expect("JSON body"));
            let expected_body: Option<Value> = expected["body"]
                .as_str()
                .map(|body| serde_json::from_str(body).expect("JSON fixture body"));
            assert_eq!(actual_body, expected_body, "{name}");
        }
    }

    #[test]
    fn admitted_response_matches_sdk_unwrapping_without_projecting_content() {
        let wire = include_str!("../tests/fixtures/wire/prompt-admitted.json");
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/transport.json"))
            .expect("fixture");
        let parsed: PromptResponse = serde_json::from_str(wire).expect("receipt");
        let sdk = &fixture["mutations"]["admitted"];
        for (name, wire) in [
            ("interrupted", r#"{"interrupted":true}"#),
            ("idle", r#"{"interrupted":false}"#),
        ] {
            let parsed: InterruptResponse =
                serde_json::from_str(wire).expect("interruption response");
            assert_eq!(
                parsed.interrupted, fixture["mutations"][name]["interrupted"],
                "SDK response parity"
            );
        }
        assert_eq!(parsed.data.id, sdk["id"]);
        assert_eq!(parsed.data.session_id, sdk["sessionID"]);
        assert_eq!(parsed.data.time.created, sdk["time"]["created"]);
        assert_eq!(
            serde_json::to_value(parsed.data.delivery).expect("delivery"),
            sdk["delivery"]
        );
        for field in ["id", "sessionID", "type", "time", "delivery"] {
            let mut body: Value = serde_json::from_str(wire).expect("wire");
            body["data"]
                .as_object_mut()
                .expect("inbox object")
                .remove(field);
            assert!(
                serde_json::from_value::<PromptResponse>(body).is_err(),
                "{field} is required for admission"
            );
        }
        for (key, value) in [
            (
                "parts",
                serde_json::json!([{ "type": "future", "text": "keep me" }]),
            ),
            (
                "files",
                serde_json::json!([{ "uri": "file:///tmp/example" }]),
            ),
            ("agents", serde_json::json!([{ "name": "plan" }])),
            ("skills", serde_json::json!([{ "id": "skill_example" }])),
            ("metadata", serde_json::json!({ "extra": true })),
        ] {
            let mut body: Value = serde_json::from_str(wire).expect("wire");
            body["data"]["payload"][key] = value;
            let parsed: PromptResponse =
                serde_json::from_value(body).expect("content is outside admission projection");
            assert_eq!(
                parsed.data.id, sdk["id"],
                "{key} does not alter admission identity"
            );
        }
        let mut body: Value = serde_json::from_str(wire).expect("wire");
        body["data"]["parts"] = serde_json::json!([{ "type": "future" }]);
        assert!(
            serde_json::from_value::<PromptResponse>(body).is_ok(),
            "additional content does not invalidate an admission receipt"
        );
    }
}
