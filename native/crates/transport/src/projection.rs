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
    BtwMetadata, MetadataFacts, SessionKind, SessionOverlay, SessionRecord,
    SessionTime as CoreSessionTime,
};
use openchamber_native_core::transcript::{
    AssistantFacts, CompactionStatus, Message, MessageInfo, Part, PartContent, PartTime, Role,
    StepOutcome, SubagentRun, SubagentRunState, SyntheticFacts, TokenUsage, ToolStatus, Usd,
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
    let MetadataFacts { overlay, kind, btw } = metadata_facts(wire.metadata.as_ref());
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
        revert: wire
            .revert
            .map(|revert| revert.message_id)
            .filter(|id| !id.is_empty())
            .map(MessageId::new),
        btw,
    }
}

/// `sessionBtwMetadata.ts`: a nonblank string, else absent.
fn btw_id(openchamber: Option<&Value>, key: &str) -> Option<String> {
    openchamber
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_owned)
}

/// The `/btw` links of an `openchamber` metadata namespace. A fork's own
/// fields count only while it is marked `kind: "btw"`.
fn btw_metadata(openchamber: Option<&Value>, is_btw: bool) -> BtwMetadata {
    BtwMetadata {
        link: btw_id(openchamber, "btwSessionID").map(SessionId::new),
        original: is_btw
            .then(|| btw_id(openchamber, "originalSessionID"))
            .flatten()
            .map(SessionId::new),
        boundary: is_btw
            .then(|| btw_id(openchamber, "btwBoundaryMessageID"))
            .flatten()
            .map(MessageId::new),
        promoted: openchamber
            .and_then(|value| value.get("btwPromoted"))
            .and_then(Value::as_bool)
            == Some(true),
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
    } else if openchamber.is_some_and(|value| {
        value.get("kind").and_then(Value::as_str) == Some("review")
            && value
                .get("originalSessionID")
                .and_then(Value::as_str)
                .is_some_and(|original| {
                    // String.trim() in sessionReviewMetadata.ts uses ECMAScript whitespace.
                    !original
                        .trim_matches(|c| {
                            matches!(c,
                    '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
                    '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' |
                    '\u{205f}' | '\u{3000}' | '\u{feff}')
                        })
                        .is_empty()
                })
    }) {
        SessionKind::Review
    } else {
        SessionKind::Conversation
    };
    MetadataFacts {
        overlay: SessionOverlay { pinned_at, in_work },
        kind,
        btw: btw_metadata(openchamber, is_btw),
    }
}

/// What a synthetic record's description and metadata establish
/// (`readSubagentRun` in `lib/opencode/subagent-run.ts`).
pub(crate) fn synthetic_facts(
    description: Option<&str>,
    metadata: Option<&Value>,
) -> Box<SyntheticFacts> {
    let text = |key: &str| {
        metadata
            .and_then(|metadata| metadata.get(key))
            .and_then(Value::as_str)
    };
    let state = match text("state") {
        Some("running") => Some(SubagentRunState::Running),
        Some("completed") => Some(SubagentRunState::Completed),
        Some("error") => Some(SubagentRunState::Error),
        Some("cancelled") => Some(SubagentRunState::Cancelled),
        _ => None,
    };
    let subagent = match (text("source"), text("childID"), state) {
        (Some("subagent"), Some(child), Some(state)) if !child.is_empty() => Some(SubagentRun {
            child: SessionId::from(child),
            agent: text("agent").map(str::to_owned),
            state,
        }),
        _ => None,
    };
    Box::new(SyntheticFacts {
        description: description.map(str::to_owned),
        subagent,
        dispatched_session: match (text("source"), text("sessionID"), text("state")) {
            (Some("openchamber-session"), Some(id), Some("completed" | "error" | "cancelled"))
                if !id.is_empty()
                    && metadata
                        .and_then(|value| value.get("title"))
                        .is_none_or(Value::is_string) =>
            {
                Some(SessionId::from(id))
            }
            _ => None,
        },
        workflow_run: match (text("source"), text("runId"), text("name"), text("state")) {
            (
                Some("openchamber-workflow"),
                Some(id),
                Some(_),
                Some("completed" | "failed" | "stopped" | "declined"),
            ) if !id.is_empty() => Some(id.to_owned()),
            _ => None,
        },
        context: crate::context_payload::read(metadata),
    })
}

#[test]
fn synthetic_report_boundaries_validate_reference_metadata() {
    let facts = |value| synthetic_facts(None, Some(&value));
    assert!(facts(serde_json::json!({"source":"openchamber-session", "sessionID":"ses_x", "state":"completed"})).dispatched_session.is_some());
    assert!(facts(serde_json::json!({"source":"openchamber-workflow", "runId":"run_x", "name":"", "state":"declined"})).workflow_run.is_some());
    for value in [
        serde_json::json!({"source":"openchamber-session", "sessionID":"", "state":"completed"}),
        serde_json::json!({"source":"openchamber-session", "sessionID":"ses_x", "state":"running"}),
        serde_json::json!({"source":"openchamber-session", "sessionID":"ses_x", "state":"error", "title":null}),
        serde_json::json!({"source":"openchamber-workflow", "runId":"run_x", "state":"completed"}),
        serde_json::json!({"source":"openchamber-workflow", "runId":"run_x", "name":"x", "state":"running"}),
    ] {
        let result = facts(value);
        assert!(result.dispatched_session.is_none() && result.workflow_run.is_none());
    }
}

/// The child session a call names (`subagentSessionId`, then the call's own
/// `sessionID` input as a resumed call carries it). Live tool events carry no
/// tool name, so the view decides which tools link to their child.
pub(crate) fn tool_child(metadata: Option<&Value>, input: Option<&Value>) -> Option<SessionId> {
    let id = |record: Option<&Value>| {
        let record = record?;
        ["sessionID", "sessionId"].into_iter().find_map(|key| {
            record
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(SessionId::from)
        })
    };
    id(metadata).or_else(|| id(input))
}

#[cfg(test)]
mod prompt_policy_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn review_requires_exact_kind_and_nonempty_original_identity() {
        for original in ["", " \t\n", "\u{feff}\u{00a0}"] {
            let metadata = json!({"openchamber":{"kind":"review","originalSessionID":original}});
            assert_eq!(
                metadata_facts(Some(&metadata)).kind,
                SessionKind::Conversation
            );
        }
        for original in ["ses_original", "\u{0085}"] {
            let metadata = json!({"openchamber":{"kind":"review","originalSessionID":original}});
            assert_eq!(metadata_facts(Some(&metadata)).kind, SessionKind::Review);
        }
        for metadata in [
            json!({"openchamber":{"kind":"review"}}),
            json!({"openchamber":{"kind":"Review","originalSessionID":"ses_original"}}),
            json!({"openchamber":{"kind":"review","originalSessionID":7}}),
            json!({"openchamber":[]}),
        ] {
            assert_eq!(
                metadata_facts(Some(&metadata)).kind,
                SessionKind::Conversation
            );
        }
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
    let mut synthetic = None;
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
            (Role::User, user_parts(&id, &user, time.created), None)
        }
        "assistant" => {
            let assistant: wire::AssistantBody = parse(body.clone(), "assistant message")?;
            let parts = assistant_parts(&id, &assistant.content, time.created, time.completed);
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
                    cost: assistant.cost.and_then(Usd::from_dollars),
                },
            });
            (Role::Assistant, parts, None)
        }
        "synthetic" => {
            synthetic = Some(synthetic_facts(
                body.get("description").and_then(Value::as_str),
                body.get("metadata"),
            ));
            (Role::Synthetic, Vec::new(), notice("text"))
        }
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
            synthetic,
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

/// A prompt's text was written at once: its time is the message's creation.
pub(crate) fn user_parts(message_id: &str, user: &wire::UserBody, created: i64) -> Vec<Part> {
    let mut parts = Vec::new();
    if !user.text.is_empty() {
        parts.push(Part {
            id: PartId::new(part_ids::text(message_id, 0)),
            content: PartContent::Text {
                text: user.text.clone(),
            },
            time: PartTime {
                start: Some(created),
                end: Some(created),
            },
        });
    }
    for (index, file) in user.files.iter().enumerate() {
        parts.push(Part {
            id: PartId::new(part_ids::user_file(message_id, index)),
            content: PartContent::File {
                filename: file.name.clone().unwrap_or_default(),
                mime: file.mime.clone(),
            },
            time: PartTime::default(),
        });
    }
    for (index, agent) in user.agents.iter().enumerate() {
        parts.push(Part {
            id: PartId::new(part_ids::user_agent(message_id, index)),
            content: PartContent::Agent {
                name: agent.name.clone(),
            },
            time: PartTime::default(),
        });
    }
    parts
}

/// Text and reasoning ordinals count per kind in content order, matching the
/// `ordinal` live `session.text.*` and `session.reasoning.*` events carry.
///
/// Times follow `projectAssistantContent`: text spans its message's window,
/// and text another item follows ended when the message was created if the
/// message is still open; reasoning keeps its own times.
pub(crate) fn assistant_parts(
    message_id: &str,
    content: &[wire::AssistantItem],
    created: i64,
    completed: Option<i64>,
) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut text_ordinal = 0;
    let mut reasoning_ordinal = 0;
    for (index, item) in content.iter().enumerate() {
        match item {
            wire::AssistantItem::Text { text } => {
                let followed = index + 1 < content.len();
                parts.push(Part {
                    id: PartId::new(part_ids::text(message_id, text_ordinal)),
                    content: PartContent::Text { text: text.clone() },
                    time: PartTime {
                        start: Some(created),
                        end: completed.or(followed.then_some(created)),
                    },
                });
                text_ordinal += 1;
            }
            wire::AssistantItem::Reasoning { text, time } => {
                parts.push(Part {
                    id: PartId::new(part_ids::reasoning(message_id, reasoning_ordinal)),
                    content: PartContent::Reasoning { text: text.clone() },
                    time: PartTime {
                        start: time
                            .as_ref()
                            .and_then(|time| time.created)
                            .or(Some(created)),
                        end: time.as_ref().and_then(|time| time.completed),
                    },
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
    let (status, output, elided, child) = match &tool.state {
        wire::ToolState::Streaming => (ToolStatus::Pending, None, None, None),
        wire::ToolState::Running { input, metadata } => (
            ToolStatus::Running,
            None,
            None,
            tool_child(metadata.as_ref(), input.as_ref()),
        ),
        wire::ToolState::Completed {
            content,
            input,
            metadata,
        } => (
            ToolStatus::Completed,
            Some(text(content)),
            elided_chars(metadata.as_ref()),
            tool_child(metadata.as_ref(), input.as_ref()),
        ),
        wire::ToolState::Error {
            error,
            content,
            input,
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
                tool_child(metadata.as_ref(), input.as_ref()),
            )
        }
    };
    // `projectToolPart`: a call runs from `ran` (else its creation) to its
    // completion (else its start); a streaming call has no time yet.
    let start = tool
        .time
        .as_ref()
        .and_then(|time| time.ran.or(time.created));
    let time = match status {
        ToolStatus::Pending => PartTime::default(),
        ToolStatus::Running => PartTime { start, end: None },
        ToolStatus::Completed | ToolStatus::Error => PartTime {
            start,
            end: tool.time.as_ref().and_then(|time| time.completed).or(start),
        },
    };
    Part {
        id: PartId::new(tool.id.clone()),
        content: PartContent::Tool {
            tool: tool.name.clone(),
            status,
            title: None,
            output,
            elided,
            child,
        },
        time,
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
        assert_eq!(
            record.btw.original.as_ref().map(SessionId::as_str),
            Some("s0")
        );
    }

    #[test]
    fn a_session_carries_its_revert_marker_and_btw_links() {
        let wire: wire::Session = serde_json::from_str(
            r#"{"id":"s1","location":{"directory":"/p"},"time":{"created":1,"updated":2},
               "revert":{"messageID":"msg_2","files":[{"file":"a"}]},
               "metadata":{"openchamber":{"btwSessionID":"ses_f","btwPromoted":true,"btwBoundaryMessageID":"msg_x"}}}"#,
        )
        .expect("valid");
        let record = session(wire);
        assert_eq!(record.revert.as_ref().map(MessageId::as_str), Some("msg_2"));
        assert_eq!(
            record.btw.link.as_ref().map(SessionId::as_str),
            Some("ses_f")
        );
        assert!(record.btw.promoted);
        assert_eq!(
            record.btw.boundary, None,
            "a boundary counts only on a marked fork"
        );
        let blank = metadata_facts(Some(
            &serde_json::json!({"openchamber":{"btwSessionID":"  "}}),
        ));
        assert_eq!(blank.btw.link, None);
    }

    #[test]
    fn synthetic_history_records_keep_their_run_and_tools_their_child() {
        let report = message(envelope(
            r#"{"id":"m4","time":{"created":1},"type":"synthetic","text":"x","description":"Explore",
                "metadata":{"source":"subagent","childID":"ses_c","state":"running"}}"#,
        ))
        .expect("parses")
        .expect("kept");
        assert_eq!(
            report
                .info
                .synthetic
                .as_ref()
                .and_then(|facts| facts.subagent.as_ref())
                .map(|run| run.child.as_str()),
            Some("ses_c")
        );
        let step = message(envelope(
            r#"{"id":"m5","time":{"created":1},"type":"assistant","content":[
                {"type":"tool","id":"call_1","name":"subagent","state":{"status":"running","input":{"description":"d"},"metadata":{}},"time":{"created":1}},
                {"type":"tool","id":"call_2","name":"subagent","state":{"status":"completed","input":{},"content":[{"type":"text","text":"ok"}],"metadata":{"sessionID":"ses_c"}},"time":{"created":1}}
            ]}"#,
        ))
        .expect("parses")
        .expect("kept");
        let children: Vec<Option<&str>> = step
            .parts
            .iter()
            .map(|part| match &part.content {
                PartContent::Tool { child, .. } => child.as_ref().map(SessionId::as_str),
                _ => None,
            })
            .collect();
        assert_eq!(children, [None, Some("ses_c")]);
    }
}
