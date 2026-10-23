//! Wire records to core records.
//!
//! Mirrors `packages/ui/src/lib/opencode/projection.ts`: part identities,
//! text and reasoning ordinals and tool states follow the same rules, so the
//! history the native client loads lines up with the live deltas that later
//! address the same parts.

use openchamber_native_core::ids::{MessageId, OpenCodeProjectId, PartId, SessionId};
use openchamber_native_core::send_config::{
    AgentEntry, AgentMode, FlatModel, FlatProvider, ModelChoice,
};
use openchamber_native_core::session::{
    MetadataFacts, SessionKind, SessionOverlay, SessionRecord, SessionTime as CoreSessionTime,
};
use openchamber_native_core::transcript::{
    AssistantFacts, CompactionStatus, Message, MessageInfo, Part, PartContent, Role, StepOutcome,
    TokenUsage, ToolStatus,
};
use serde_json::Value;

use crate::error::TransportError;
use crate::wire;

/// Metadata key of a finished call whose result a shelled page left out.
const CONTENT_SHELL_KEY: &str = "openchamberContentShell";

/// Deterministic part identities shared with live events.
pub(crate) mod part_ids {
    pub fn text(message_id: &str, ordinal: usize) -> String {
        format!("{message_id}:text:{ordinal}")
    }

    pub fn reasoning(message_id: &str, ordinal: usize) -> String {
        format!("{message_id}:reasoning:{ordinal}")
    }

    pub fn user_file(message_id: &str, index: usize) -> String {
        format!("{message_id}:file:{index}")
    }

    pub fn user_agent(message_id: &str, index: usize) -> String {
        format!("{message_id}:agent:{index}")
    }
}

/// Projects a session record with its OpenChamber overlay.
pub(crate) fn session(wire: wire::Session) -> SessionRecord {
    let MetadataFacts { overlay, kind } = metadata_facts(wire.metadata.as_ref());
    SessionRecord {
        id: SessionId::new(wire.id),
        parent_id: wire
            .parent_id
            .filter(|id| !id.is_empty())
            .map(SessionId::new),
        directory: Some(wire.location.directory),
        title: wire.title.unwrap_or_default(),
        time: CoreSessionTime {
            created: wire.time.created,
            updated: wire.time.updated,
            idle: wire.time.idle,
            archived: wire.time.archived,
        },
        project_id: wire.project_id.map(OpenCodeProjectId::new),
        project: None,
        kind,
        overlay,
        model: wire.model.map(model_choice),
        agent: wire.agent.filter(|agent| !agent.is_empty()),
    }
}

/// The OpenChamber overlay and session kind from a session's full metadata.
pub(crate) fn metadata_facts(metadata: Option<&Value>) -> MetadataFacts {
    let openchamber = metadata
        .and_then(|metadata| metadata.get("openchamber"))
        .filter(|value| value.is_object());
    let pinned_at = openchamber
        .and_then(|value| value.pointer("/pinned/at"))
        .and_then(Value::as_i64);
    let in_work = openchamber
        .and_then(|value| value.pointer("/work/state"))
        .and_then(Value::as_str)
        == Some("open");
    let is_btw = openchamber.is_some_and(|value| {
        value.get("kind").and_then(Value::as_str) == Some("btw")
            && value
                .get("originalSessionID")
                .and_then(Value::as_str)
                .is_some_and(|original| !original.trim().is_empty())
    });
    let kind = if is_btw {
        SessionKind::Btw
    } else {
        SessionKind::Conversation
    };
    MetadataFacts {
        overlay: SessionOverlay { pinned_at, in_work },
        kind,
    }
}

/// Projects one message record; unknown message types are skipped.
pub(crate) fn message(envelope: wire::MessageEnvelope) -> Result<Option<Message>, TransportError> {
    let wire::MessageEnvelope {
        id,
        time,
        kind,
        body,
    } = envelope;
    let body = Value::Object(body);
    let notice = |field: &str| body.get(field).and_then(Value::as_str).map(str::to_owned);
    let mut facts = None;
    let compaction = (kind == "compaction")
        .then(|| match body.get("status").and_then(Value::as_str) {
            Some("running") => Some(CompactionStatus::Running),
            Some("completed") => Some(CompactionStatus::Completed),
            Some("failed") => Some(CompactionStatus::Failed),
            _ => None,
        })
        .flatten();
    let (role, parts, notice) = match kind.as_str() {
        "user" => {
            let user: wire::UserBody = parse(body.clone(), "user message")?;
            (Role::User, user_parts(&id, &user), None)
        }
        "assistant" => {
            let assistant: wire::AssistantBody = parse(body.clone(), "assistant message")?;
            let parts = assistant_parts(&id, &assistant.content);
            facts = Some(AssistantFacts {
                agent: assistant.agent.filter(|agent| !agent.is_empty()),
                model: assistant.model.map(model_choice),
                outcome: StepOutcome {
                    finish: assistant.finish,
                    error: assistant
                        .error
                        .as_ref()
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    tokens: assistant.tokens.map(token_usage),
                },
            });
            (Role::Assistant, parts, None)
        }
        "synthetic" => (Role::Synthetic, Vec::new(), notice("text")),
        "system" => (Role::System, Vec::new(), notice("text")),
        "skill" => (Role::Skill, Vec::new(), notice("name")),
        "shell" => (Role::Shell, Vec::new(), notice("command")),
        "compaction" => (Role::Compaction, Vec::new(), notice("summary")),
        "agent-switched" => (Role::AgentSwitched, Vec::new(), notice("agent")),
        "model-switched" => (
            Role::ModelSwitched,
            Vec::new(),
            body.pointer("/model/id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ),
        "location-switched" => (
            Role::LocationSwitched,
            Vec::new(),
            body.pointer("/location/directory")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ),
        "idle" => (Role::Idle, Vec::new(), notice("outcome")),
        other => {
            tracing::debug!(message_type = other, "skipping an unknown message type");
            return Ok(None);
        }
    };
    Ok(Some(Message {
        info: MessageInfo {
            id: MessageId::new(id),
            role,
            created: time.created,
            completed: time.completed,
            notice,
            assistant: facts,
            compaction,
        },
        parts,
    }))
}

pub(crate) fn token_usage(wire: wire::TokenUsage) -> TokenUsage {
    TokenUsage {
        input: wire.input,
        output: wire.output,
        reasoning: wire.reasoning,
        cache_read: wire.cache.read,
        cache_write: wire.cache.write,
    }
}

fn parse<T: serde::de::DeserializeOwned>(value: Value, what: &str) -> Result<T, TransportError> {
    serde_json::from_value(value)
        .map_err(|error| TransportError::Malformed(format!("{what}: {error}")))
}

pub(crate) fn user_parts(message_id: &str, user: &wire::UserBody) -> Vec<Part> {
    let mut parts = Vec::new();
    if !user.text.is_empty() {
        parts.push(Part::text(
            PartId::new(part_ids::text(message_id, 0)),
            user.text.clone(),
        ));
    }
    for (index, file) in user.files.iter().enumerate() {
        parts.push(Part {
            id: PartId::new(part_ids::user_file(message_id, index)),
            content: PartContent::File {
                filename: file.name.clone().unwrap_or_default(),
                mime: file.mime.clone(),
            },
        });
    }
    for (index, agent) in user.agents.iter().enumerate() {
        parts.push(Part {
            id: PartId::new(part_ids::user_agent(message_id, index)),
            content: PartContent::Agent {
                name: agent.name.clone(),
            },
        });
    }
    parts
}

/// Text and reasoning ordinals count per kind in content order, matching the
/// `ordinal` live `session.text.*` and `session.reasoning.*` events carry.
pub(crate) fn assistant_parts(message_id: &str, content: &[wire::AssistantItem]) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut text_ordinal = 0;
    let mut reasoning_ordinal = 0;
    for item in content {
        match item {
            wire::AssistantItem::Text { text } => {
                parts.push(Part::text(
                    PartId::new(part_ids::text(message_id, text_ordinal)),
                    text.clone(),
                ));
                text_ordinal += 1;
            }
            wire::AssistantItem::Reasoning { text } => {
                parts.push(Part {
                    id: PartId::new(part_ids::reasoning(message_id, reasoning_ordinal)),
                    content: PartContent::Reasoning { text: text.clone() },
                });
                reasoning_ordinal += 1;
            }
            wire::AssistantItem::Tool(tool) => parts.push(tool_part(tool)),
            wire::AssistantItem::Unsupported => {}
        }
    }
    parts
}

pub(crate) fn tool_part(tool: &wire::ToolItem) -> Part {
    let text = |content: &[wire::ToolContent]| -> String {
        let texts: Vec<&str> = content
            .iter()
            .filter_map(|item| match item {
                wire::ToolContent::Text { text } => Some(text.as_str()),
                wire::ToolContent::Other => None,
            })
            .collect();
        texts.join("\n")
    };
    let (status, output, elided) = match &tool.state {
        wire::ToolState::Streaming => (ToolStatus::Pending, None, None),
        wire::ToolState::Running => (ToolStatus::Running, None, None),
        wire::ToolState::Completed { content, metadata } => (
            ToolStatus::Completed,
            Some(text(content)),
            elided_chars(metadata.as_ref()),
        ),
        wire::ToolState::Error {
            error,
            content,
            metadata,
        } => {
            let output = text(content);
            let message = if error.message.is_empty() {
                error.kind.clone()
            } else {
                error.message.clone()
            };
            (
                ToolStatus::Error,
                Some(if output.is_empty() { message } else { output }),
                elided_chars(metadata.as_ref()),
            )
        }
    };
    Part {
        id: PartId::new(tool.id.clone()),
        content: PartContent::Tool {
            tool: tool.name.clone(),
            status,
            title: None,
            output,
            elided,
        },
    }
}

/// The length of a result a shelled page left out, as
/// `toolContentShellChars` in `lib/opencode/toolContentShells.ts` reads it.
fn elided_chars(metadata: Option<&Value>) -> Option<usize> {
    let chars = metadata?.get(CONTENT_SHELL_KEY)?.get("chars")?.as_f64()?;
    // Lengths are whole numbers; anything else is a malformed marker.
    (chars >= 0.0 && chars.fract() == 0.0).then_some(chars as usize)
}

/// Projects an agent as `projectAgent` does: the ID becomes the name, the
/// name the display name. Hidden is either flag, as `isAgentHidden` reads.
pub(crate) fn agent(wire: wire::AgentInfo) -> AgentEntry {
    let hidden_option = wire
        .options
        .as_ref()
        .and_then(|options| options.get("hidden"))
        .and_then(Value::as_bool);
    AgentEntry {
        name: wire.id,
        display_name: wire.name,
        mode: match wire.mode.as_deref() {
            Some("primary") => Some(AgentMode::Primary),
            Some("subagent") => Some(AgentMode::Subagent),
            Some("all") => Some(AgentMode::All),
            _ => None,
        },
        hidden: wire.hidden == Some(true) || hidden_option == Some(true),
        model: wire.model.map(model_choice),
        color: wire.color,
    }
}

pub(crate) fn model_choice(wire: wire::AgentModel) -> ModelChoice {
    ModelChoice {
        provider_id: wire.provider_id,
        model_id: wire.id,
        variant: wire.variant,
    }
}

/// Projects one model of the flat catalog.
pub(crate) fn model(wire: wire::ModelInfo) -> FlatModel {
    FlatModel {
        provider_model_id: (wire.model_id != wire.id).then_some(wire.model_id),
        id: wire.id,
        provider_id: wire.provider_id,
        name: wire.name,
        variants: wire
            .variants
            .into_iter()
            .flatten()
            .map(|variant| variant.id)
            .filter(|id| !id.is_empty())
            .collect(),
        context_limit: wire
            .limit
            .and_then(|limit| limit.context)
            .filter(|context| context.is_finite() && *context >= 1.0)
            .map(|context| context as u64),
        enabled: wire.enabled,
    }
}

/// Projects a provider; only an explicit `disabled` activation turns it off.
pub(crate) fn provider(wire: wire::ProviderInfo) -> FlatProvider {
    FlatProvider {
        enabled: wire.activation.as_deref() != Some("disabled"),
        id: wire.id,
        name: wire.name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(json: &str) -> wire::MessageEnvelope {
        serde_json::from_str(json).expect("test JSON is valid")
    }

    #[test]
    fn a_model_keeps_only_a_positive_context_window() {
        let limit = |json: &str| {
            let wire: wire::ModelInfo = serde_json::from_str(json).expect("test JSON is valid");
            model(wire).context_limit
        };
        let base = r#""id":"m","modelID":"m","providerID":"p","enabled":true"#;
        assert_eq!(
            limit(&format!(
                r#"{{{base},"limit":{{"context":400000,"output":8000}}}}"#
            )),
            Some(400_000)
        );
        assert_eq!(
            limit(&format!(r#"{{{base},"limit":{{"context":0,"output":0}}}}"#)),
            None
        );
        assert_eq!(limit(&format!("{{{base}}}")), None);
    }

    #[test]
    fn assistant_ordinals_count_per_kind() {
        let message = message(envelope(
            r#"{"id":"m1","time":{"created":1,"completed":2},"type":"assistant","content":[
                {"type":"reasoning","text":"think"},
                {"type":"text","text":"a"},
                {"type":"tool","id":"call_1","name":"read","state":{"status":"completed","content":[{"type":"text","text":"ok"}]},"time":{"created":1}},
                {"type":"text","text":"b"}
            ]}"#,
        ))
        .expect("parses")
        .expect("assistant messages are kept");
        let ids: Vec<&str> = message.parts.iter().map(|part| part.id.as_str()).collect();
        assert_eq!(ids, ["m1:reasoning:0", "m1:text:0", "call_1", "m1:text:1"]);
        assert_eq!(message.info.completed, Some(2));
    }

    #[test]
    fn notices_and_unknown_types() {
        let switched = message(envelope(
            r#"{"id":"m2","time":{"created":1},"type":"agent-switched","agent":"plan","previous":"build"}"#,
        ))
        .expect("parses")
        .expect("kept");
        assert_eq!(switched.info.role, Role::AgentSwitched);
        assert_eq!(switched.info.notice.as_deref(), Some("plan"));
        let unknown = message(envelope(
            r#"{"id":"m3","time":{"created":1},"type":"future"}"#,
        ))
        .expect("unknown types are not errors");
        assert!(unknown.is_none());
    }

    #[test]
    fn session_overlay_reads_pins_work_and_btw() {
        let wire: wire::Session = serde_json::from_str(
            r#"{"id":"s1","location":{"directory":"/p"},"time":{"created":1,"updated":2,"archived":3},
               "metadata":{"openchamber":{"pinned":{"at":5},"work":{"state":"open"},"kind":"btw","originalSessionID":"s0"}}}"#,
        )
        .expect("valid");
        let record = session(wire);
        assert_eq!(record.overlay.pinned_at, Some(5));
        assert!(record.overlay.in_work);
        assert!(record.is_btw());
        assert!(record.is_archived());
        assert_eq!(record.title, "");
    }
}
