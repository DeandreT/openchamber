//! Frames of the server's global event bridge, translated into domain events.
//!
//! Mirrors `translateWireEvent` in `packages/ui/src/lib/opencode/events.ts`
//! and the OpenChamber frame handling in `sync/event-pipeline.ts` for the
//! session and transcript events the native client models. Every other type
//! becomes `ServerEvent::Unsupported` with its name only, never a fabricated
//! state change.

use openchamber_native_core::ids::{MessageId, OpenCodeProjectId, PartId, SessionId};
use openchamber_native_core::session::{LiveStatus, SessionPatch, SessionRecord, SessionTime};
use openchamber_native_core::session_index::SessionChange;
use openchamber_native_core::transcript::{
    AssistantFacts, CompactionStatus, MessageInfo, Part, PartContent, PartTime, Role, StepOutcome,
    ToolStatus, TranscriptEvent, Usd,
};
use serde::Deserialize;
use serde_json::Value;

use crate::error::TransportError;
use crate::projection::{self, metadata_facts, part_ids};
use crate::request_wire;
use crate::wire;
use openchamber_native_core::pending_requests::{PendingRequest, RequestId};
use openchamber_native_core::request_owner::RequestOwner;

/// Live blocking-request transitions. Terminal events settle only their exact owner and ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockingRequestEvent {
    Asked(PendingRequest),
    Settled { owner: RequestOwner, id: RequestId },
}

/// One domain change announced by the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerEvent {
    /// A permission or form transition. Real-session events can omit location;
    /// location-owned forms always retain an explicit directory.
    BlockingRequest {
        directory: Option<String>,
        event: BlockingRequestEvent,
    },
    /// A malformed blocking event grants no state authority. Reconcile its known scope;
    /// keep consuming the stream so one bad request cannot strand unrelated sessions.
    BlockingRequestInvalid {
        kind: String,
        directory: Option<String>,
    },
    /// The location's live services stopped. Reconcile selected scopes without project fan-out.
    LocationShutdown {
        directory: String,
    },
    /// The bridge is ready; `replay_reset` means replay could not resume.
    Ready {
        replay_reset: bool,
    },
    SessionCreated(SessionRecord),
    SessionPatched {
        session_id: SessionId,
        patch: SessionPatch,
    },
    SessionDeleted {
        session_id: SessionId,
    },
    SessionStatus {
        session_id: SessionId,
        status: LiveStatus,
    },
    Transcript {
        session_id: SessionId,
        event: TranscriptEvent,
    },
    /// OpenCode rebuilt a catalog in `directory`, or in every directory when
    /// the event names none.
    CatalogUpdated {
        kind: CatalogKind,
        directory: Option<String>,
    },
    Heartbeat,
    /// The server is shedding load; reconcile after it recovers.
    Backpressure,
    /// A type this client does not model.
    Unsupported {
        kind: String,
    },
}

/// Which catalog an update rebuilt, as `catalog.updated` kinds in
/// `packages/ui/src/lib/opencode/events.ts` name them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CatalogKind {
    Config,
    Agent,
    Skill,
    Provider,
    Model,
    Credential,
    /// An MCP server's connection state changed.
    McpStatus,
}

impl CatalogKind {
    fn from_event_type(kind: &str) -> Option<Self> {
        Some(match kind {
            "config.updated" => Self::Config,
            "agent.updated" => Self::Agent,
            "skill.updated" => Self::Skill,
            "provider.updated" => Self::Provider,
            "model.updated" => Self::Model,
            "credential.updated" | "credential.switched" => Self::Credential,
            "mcp.status.changed" => Self::McpStatus,
            _ => return None,
        })
    }
}

impl ServerEvent {
    /// The change this event makes to the global session index, if any.
    ///
    /// The end of a turn settles the session, so a deferred recency update
    /// applies.
    pub fn session_change(&self) -> Option<SessionChange> {
        match self {
            Self::SessionCreated(record) => Some(SessionChange::Created(record.clone())),
            Self::SessionPatched { session_id, patch } => Some(SessionChange::Patched {
                session_id: session_id.clone(),
                patch: patch.clone(),
            }),
            Self::SessionDeleted { session_id } => Some(SessionChange::Deleted(session_id.clone())),
            Self::SessionStatus {
                session_id,
                status: LiveStatus::Idle,
            } => Some(SessionChange::Settled(session_id.clone())),
            _ => None,
        }
    }
}

/// One decoded bridge frame.
#[derive(Debug, PartialEq, Eq)]
pub struct DecodedFrame {
    /// The replay identifier to resume from after a reconnect.
    pub event_id: Option<String>,
    pub events: Vec<ServerEvent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    replay_reset: bool,
    payload: Option<Value>,
    event_id: Option<String>,
    message: Option<String>,
}

/// Decodes one text frame of `/api/global/event/ws`.
pub fn decode_frame(text: &str) -> Result<DecodedFrame, TransportError> {
    let frame: Frame = serde_json::from_str(text)
        .map_err(|error| TransportError::Malformed(format!("event frame: {error}")))?;
    let events = match frame.kind.as_str() {
        "ready" => vec![ServerEvent::Ready {
            replay_reset: frame.replay_reset,
        }],
        "backpressure" => vec![ServerEvent::Backpressure],
        "error" => {
            return Err(TransportError::Stream(frame.message.unwrap_or_else(|| {
                "the server reported a stream error".to_owned()
            })));
        }
        "event" => match frame.payload {
            Some(payload) => match blocking_event(&payload) {
                Ok(Some(event)) => vec![event],
                Ok(None) => translate(&payload),
                Err(_) => vec![ServerEvent::BlockingRequestInvalid {
                    kind: payload
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    directory: payload
                        .pointer("/location/directory")
                        .and_then(Value::as_str)
                        .and_then(openchamber_native_core::request_owner::normalize_directory),
                }],
            },
            None => Vec::new(),
        },
        other => vec![ServerEvent::Unsupported {
            kind: format!("frame:{other}"),
        }],
    };
    Ok(DecodedFrame {
        event_id: frame.event_id,
        events,
    })
}

fn blocking_event(payload: &Value) -> Result<Option<ServerEvent>, TransportError> {
    use openchamber_native_core::ids::{FormId, PermissionId};
    let kind = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !matches!(
        kind,
        "permission.asked"
            | "permission.replied"
            | "form.created"
            | "form.replied"
            | "form.cancelled"
            | "location.shutdown"
    ) {
        return Ok(None);
    }
    let malformed = || TransportError::Malformed("invalid blocking request event".to_owned());
    let directory = match payload.get("location") {
        None => None,
        Some(location) => {
            let value = location
                .get("directory")
                .and_then(Value::as_str)
                .ok_or_else(malformed)?;
            Some(
                openchamber_native_core::request_owner::normalize_directory(value)
                    .ok_or_else(malformed)?,
            )
        }
    };
    if kind == "location.shutdown" {
        return Ok(Some(ServerEvent::LocationShutdown {
            directory: directory.ok_or_else(malformed)?,
        }));
    }
    let data = payload.get("data").ok_or_else(malformed)?;
    let event = match kind {
        "permission.asked" => {
            let value: request_wire::Permission =
                serde_json::from_value(data.clone()).map_err(|_| malformed())?;
            BlockingRequestEvent::Asked(PendingRequest::Permission(
                value.project().map_err(|_| malformed())?,
            ))
        }
        "form.created" => {
            let value: request_wire::Form =
                serde_json::from_value(data.get("form").ok_or_else(malformed)?.clone())
                    .map_err(|_| malformed())?;
            BlockingRequestEvent::Asked(PendingRequest::Form(
                value.project().map_err(|_| malformed())?,
            ))
        }
        _ => {
            let owner = RequestOwner::from_wire(
                data.get("sessionID")
                    .and_then(Value::as_str)
                    .ok_or_else(malformed)?,
            )
            .ok_or_else(malformed)?;
            let id = if kind == "permission.replied" {
                if !matches!(owner, RequestOwner::Session(_))
                    || !matches!(
                        data.get("reply").and_then(Value::as_str),
                        Some("once" | "always" | "reject")
                    )
                {
                    return Err(malformed());
                }
                let id = data
                    .get("requestID")
                    .and_then(Value::as_str)
                    .ok_or_else(malformed)?;
                if !request_wire::valid_id(id, "per") {
                    return Err(malformed());
                }
                RequestId::Permission(PermissionId::from(id))
            } else {
                let id = data
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(malformed)?;
                if !request_wire::valid_id(id, "frm_") {
                    return Err(malformed());
                }
                if kind == "form.replied" {
                    let _: std::collections::BTreeMap<String, request_wire::AnswerValue> =
                        serde_json::from_value(data.get("answer").ok_or_else(malformed)?.clone())
                            .map_err(|_| malformed())?;
                }
                RequestId::Form(FormId::from(id))
            };
            BlockingRequestEvent::Settled { owner, id }
        }
    };
    let global = match &event {
        BlockingRequestEvent::Asked(PendingRequest::Form(form)) => {
            matches!(form.owner, RequestOwner::Location)
        }
        BlockingRequestEvent::Settled { owner, .. } => matches!(owner, RequestOwner::Location),
        _ => false,
    };
    if global && directory.is_none() {
        return Err(malformed());
    }
    Ok(Some(ServerEvent::BlockingRequest { directory, event }))
}

/// Server events and the messages they create share one id space.
fn message_id_from_event(event_id: &str) -> MessageId {
    MessageId::new(match event_id.strip_prefix("evt_") {
        Some(rest) => format!("msg_{rest}"),
        None => event_id.to_owned(),
    })
}

fn translate(payload: &Value) -> Vec<ServerEvent> {
    let kind = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(events) = translate_openchamber(kind, payload) {
        return events;
    }
    if let Some(kind) = CatalogKind::from_event_type(kind) {
        return vec![ServerEvent::CatalogUpdated {
            kind,
            directory: payload
                .pointer("/location/directory")
                .and_then(Value::as_str)
                .filter(|directory| !directory.is_empty() && *directory != "global")
                .map(str::to_owned),
        }];
    }
    let data = payload.get("data").unwrap_or(&Value::Null);
    let created = payload
        .get("created")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let event_id = payload
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = |field: &str| data.get(field).and_then(Value::as_str);
    let Some(session_id) = text("sessionID").map(SessionId::from) else {
        return vec![ServerEvent::Unsupported {
            kind: kind.to_owned(),
        }];
    };
    let transcript = |event: TranscriptEvent| ServerEvent::Transcript {
        session_id: session_id.clone(),
        event,
    };
    let notice = |role: Role, notice: Option<&str>| {
        transcript(TranscriptEvent::MessageUpdated(MessageInfo {
            id: message_id_from_event(event_id),
            role,
            created,
            completed: None,
            notice: notice.map(str::to_owned),
            assistant: None,
            compaction: None,
        }))
    };
    let assistant = || text("assistantMessageID").map(MessageId::from);
    let model = || {
        data.get("model")
            .and_then(|model| wire::AgentModel::deserialize(model).ok())
            .map(projection::model_choice)
    };
    let ordinal = || {
        data.get("ordinal")
            .and_then(Value::as_u64)
            .unwrap_or_default() as usize
    };
    let status = |status: LiveStatus| ServerEvent::SessionStatus {
        session_id: session_id.clone(),
        status,
    };
    let settled = |patch: SessionPatch| {
        vec![
            ServerEvent::SessionPatched {
                session_id: session_id.clone(),
                patch,
            },
            status(LiveStatus::Idle),
        ]
    };

    match kind {
        "session.created" => {
            let facts = metadata_facts(data.get("metadata"));
            vec![ServerEvent::SessionCreated(SessionRecord {
                id: session_id.clone(),
                parent_id: text("parentID")
                    .filter(|id| !id.is_empty())
                    .map(SessionId::from),
                directory: data
                    .pointer("/location/directory")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                title: text("title").unwrap_or_default().to_owned(),
                time: SessionTime {
                    created,
                    updated: created,
                    idle: None,
                    archived: None,
                },
                project_id: text("projectID").map(Into::into),
                project: None,
                kind: facts.kind,
                overlay: facts.overlay,
                model: model(),
                agent: text("agent")
                    .filter(|agent| !agent.is_empty())
                    .map(str::to_owned),
            })]
        }
        "session.deleted" => vec![ServerEvent::SessionDeleted { session_id }],
        "session.renamed" => vec![ServerEvent::SessionPatched {
            session_id: session_id.clone(),
            patch: SessionPatch {
                title: text("title").map(str::to_owned),
                updated: Some(created),
                ..SessionPatch::default()
            },
        }],
        "session.metadata.updated" => vec![ServerEvent::SessionPatched {
            session_id: session_id.clone(),
            patch: SessionPatch {
                metadata: Some(metadata_facts(data.get("metadata"))),
                ..SessionPatch::default()
            },
        }],
        "session.usage.updated" => vec![ServerEvent::SessionPatched {
            session_id: session_id.clone(),
            patch: SessionPatch {
                updated: Some(created),
                ..SessionPatch::default()
            },
        }],
        "session.status" => match data.pointer("/status/type").and_then(Value::as_str) {
            Some("busy") => vec![status(LiveStatus::Busy)],
            Some("retry") => vec![status(LiveStatus::Retry)],
            Some("idle") => vec![status(LiveStatus::Idle)],
            _ => Vec::new(),
        },
        "session.moved" => {
            let directory = data
                .pointer("/location/directory")
                .and_then(Value::as_str)
                .map(str::to_owned);
            vec![
                ServerEvent::SessionPatched {
                    session_id: session_id.clone(),
                    patch: SessionPatch {
                        directory: directory.clone(),
                        project_id: text("projectID").map(OpenCodeProjectId::from),
                        updated: Some(created),
                        ..SessionPatch::default()
                    },
                },
                notice(Role::LocationSwitched, directory.as_deref()),
            ]
        }
        "session.agent.selected" => vec![
            ServerEvent::SessionPatched {
                session_id: session_id.clone(),
                patch: SessionPatch {
                    updated: Some(created),
                    agent: text("agent")
                        .filter(|agent| !agent.is_empty())
                        .map(str::to_owned),
                    ..SessionPatch::default()
                },
            },
            notice(Role::AgentSwitched, text("agent")),
        ],
        "session.model.selected" => vec![
            ServerEvent::SessionPatched {
                session_id: session_id.clone(),
                patch: SessionPatch {
                    updated: Some(created),
                    model: model(),
                    ..SessionPatch::default()
                },
            },
            notice(
                Role::ModelSwitched,
                data.pointer("/model/id").and_then(Value::as_str),
            ),
        ],
        "session.synthetic" => vec![notice(Role::Synthetic, text("text"))],
        "session.skill.activated" => vec![notice(Role::Skill, text("name"))],
        "session.instructions.updated" => match text("text") {
            Some(instructions) => vec![notice(Role::System, Some(instructions))],
            None => Vec::new(),
        },
        "session.execution.started" => vec![status(LiveStatus::Busy)],
        "session.execution.succeeded" | "session.execution.failed" => settled(SessionPatch {
            idle: Some(created),
            updated: Some(created),
            ..SessionPatch::default()
        }),
        // `shutdown` is OpenCode going away mid-turn; it resumes on restart, so
        // the status snapshot after reconnect is the authority.
        "session.execution.interrupted" if text("reason") == Some("shutdown") => Vec::new(),
        "session.execution.interrupted" => settled(SessionPatch {
            idle: Some(created),
            updated: Some(created),
            ..SessionPatch::default()
        }),
        "session.idle" => vec![status(LiveStatus::Idle)],
        "session.inbox.enqueued" => {
            let Some(inbox_id) = text("inboxID") else {
                return Vec::new();
            };
            let message_id = MessageId::from(inbox_id);
            let info = |role: Role, notice: Option<String>| MessageInfo {
                id: message_id.clone(),
                role,
                created,
                completed: None,
                notice,
                assistant: None,
                compaction: None,
            };
            let payload = data
                .pointer("/item/payload")
                .cloned()
                .unwrap_or(Value::Null);
            match data.pointer("/item/type").and_then(Value::as_str) {
                Some("user") => {
                    let Ok(user) = serde_json::from_value::<wire::UserBody>(payload) else {
                        return vec![ServerEvent::Unsupported {
                            kind: kind.to_owned(),
                        }];
                    };
                    vec![
                        transcript(TranscriptEvent::MessageUpdated(info(Role::User, None))),
                        transcript(TranscriptEvent::PartsReplaced {
                            message_id: message_id.clone(),
                            parts: projection::user_parts(inbox_id, &user, created),
                        }),
                    ]
                }
                Some("synthetic") => {
                    let notice = payload
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    vec![transcript(TranscriptEvent::MessageUpdated(info(
                        Role::Synthetic,
                        notice,
                    )))]
                }
                _ => Vec::new(),
            }
        }
        "session.inbox.delivered" => match text("inboxID") {
            Some(inbox_id) => vec![transcript(TranscriptEvent::MessageDelivered {
                message_id: MessageId::from(inbox_id),
                created,
            })],
            None => Vec::new(),
        },
        // `events.ts`: a compaction's start and failure take the input's ID
        // when there is one; its end is a record of its own.
        "session.compaction.started" | "session.compaction.ended" | "session.compaction.failed" => {
            let (status, id, summary) = match kind {
                "session.compaction.started" => {
                    (CompactionStatus::Running, text("inputID"), Some(""))
                }
                "session.compaction.ended" => (CompactionStatus::Completed, None, text("text")),
                _ => (CompactionStatus::Failed, text("inputID"), Some("")),
            };
            vec![transcript(TranscriptEvent::MessageUpdated(MessageInfo {
                id: id.map_or_else(|| message_id_from_event(event_id), MessageId::from),
                role: Role::Compaction,
                created,
                completed: None,
                notice: summary.map(str::to_owned),
                assistant: None,
                compaction: Some(status),
            }))]
        }
        "session.inbox.cancelled" => match text("inboxID") {
            Some(inbox_id) => vec![transcript(TranscriptEvent::MessageRemoved {
                message_id: MessageId::from(inbox_id),
            })],
            None => Vec::new(),
        },
        "session.step.started" => match assistant() {
            Some(message_id) => vec![transcript(TranscriptEvent::MessageUpdated(MessageInfo {
                id: message_id,
                role: Role::Assistant,
                created,
                completed: None,
                notice: None,
                assistant: Some(AssistantFacts {
                    agent: text("agent")
                        .filter(|agent| !agent.is_empty())
                        .map(str::to_owned),
                    model: model(),
                    outcome: StepOutcome::default(),
                }),
                compaction: None,
            }))],
            None => Vec::new(),
        },
        "session.step.ended" | "session.step.failed" => match assistant() {
            Some(message_id) => {
                let failed = kind == "session.step.failed";
                let finish = text("finish")
                    .map(str::to_owned)
                    .or_else(|| failed.then(|| "error".to_owned()));
                let error = data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let tokens = data
                    .get("tokens")
                    .and_then(|tokens| wire::TokenUsage::deserialize(tokens).ok())
                    .map(projection::token_usage);
                let cost = data
                    .get("cost")
                    .and_then(Value::as_f64)
                    .and_then(Usd::from_dollars);
                vec![transcript(TranscriptEvent::MessageCompleted {
                    message_id,
                    completed: created,
                    outcome: StepOutcome {
                        finish,
                        error,
                        tokens,
                        cost,
                    },
                })]
            }
            None => Vec::new(),
        },
        "session.text.started"
        | "session.text.ended"
        | "session.reasoning.started"
        | "session.reasoning.ended" => {
            let Some(message_id) = assistant() else {
                return Vec::new();
            };
            let full_text = text("text").unwrap_or_default().to_owned();
            let content = if kind.starts_with("session.text") {
                PartContent::Text { text: full_text }
            } else {
                PartContent::Reasoning { text: full_text }
            };
            let id = if kind.starts_with("session.text") {
                part_ids::text(message_id.as_str(), ordinal())
            } else {
                part_ids::reasoning(message_id.as_str(), ordinal())
            };
            // A part starts when its stream starts; an `ended` snapshot stands
            // in for both ends when the stream was joined late, and the
            // transcript keeps an earlier streamed start.
            let ended = kind.ends_with(".ended");
            vec![transcript(TranscriptEvent::PartUpdated {
                message_id,
                part: Part {
                    id: PartId::new(id),
                    content,
                    time: PartTime {
                        start: Some(created),
                        end: ended.then_some(created),
                    },
                },
            })]
        }
        "session.text.delta" | "session.reasoning.delta" => {
            let Some(message_id) = assistant() else {
                return Vec::new();
            };
            let id = if kind == "session.text.delta" {
                part_ids::text(message_id.as_str(), ordinal())
            } else {
                part_ids::reasoning(message_id.as_str(), ordinal())
            };
            vec![transcript(TranscriptEvent::PartDelta {
                message_id,
                part_id: PartId::new(id),
                delta: text("delta").unwrap_or_default().to_owned(),
            })]
        }
        "session.tool.input.started" => match (assistant(), text("id")) {
            (Some(message_id), Some(call_id)) => vec![transcript(TranscriptEvent::PartUpdated {
                message_id,
                part: Part {
                    id: PartId::from(call_id),
                    content: PartContent::Tool {
                        tool: text("name").unwrap_or_default().to_owned(),
                        status: ToolStatus::Pending,
                        title: None,
                        output: None,
                        elided: None,
                    },
                    time: PartTime::default(),
                },
            })],
            _ => Vec::new(),
        },
        "session.tool.called" | "session.tool.success" | "session.tool.failed" => {
            let (Some(message_id), Some(call_id)) = (assistant(), text("id")) else {
                return Vec::new();
            };
            let content_text = tool_text(data.get("content"));
            let (status, output) = match kind {
                "session.tool.called" => (ToolStatus::Running, None),
                "session.tool.success" => (ToolStatus::Completed, Some(content_text)),
                _ => {
                    let error = data
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .filter(|message| !message.is_empty())
                        .or_else(|| data.pointer("/error/type").and_then(Value::as_str))
                        .unwrap_or_default()
                        .to_owned();
                    (
                        ToolStatus::Error,
                        Some(if content_text.is_empty() {
                            error
                        } else {
                            content_text
                        }),
                    )
                }
            };
            vec![transcript(TranscriptEvent::ToolTransition {
                message_id,
                part_id: PartId::from(call_id),
                status,
                output,
                at: created,
            })]
        }
        _ => vec![ServerEvent::Unsupported {
            kind: kind.to_owned(),
        }],
    }
}

/// OpenChamber's own events on the bridge.
fn translate_openchamber(kind: &str, payload: &Value) -> Option<Vec<ServerEvent>> {
    if !kind.starts_with("openchamber:") {
        return None;
    }
    let properties = payload.get("properties").unwrap_or(&Value::Null);
    let session_id = properties
        .get("sessionID")
        .or_else(|| properties.get("sessionId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(SessionId::from);
    let events = match (kind, session_id) {
        ("openchamber:heartbeat", _) => vec![ServerEvent::Heartbeat],
        ("openchamber:session-status", Some(session_id)) => {
            let status = match properties.get("status").and_then(Value::as_str) {
                Some("busy") => LiveStatus::Busy,
                Some("idle") => LiveStatus::Idle,
                // A retry without its attempt, message and next time is not a
                // valid status, as `translateOpenchamberStatus` decides.
                Some("retry")
                    if properties
                        .pointer("/metadata/attempt")
                        .is_some_and(Value::is_number)
                        && properties
                            .pointer("/metadata/message")
                            .is_some_and(Value::is_string)
                        && properties
                            .pointer("/metadata/next")
                            .is_some_and(Value::is_number) =>
                {
                    LiveStatus::Retry
                }
                _ => return Some(Vec::new()),
            };
            vec![ServerEvent::SessionStatus { session_id, status }]
        }
        ("openchamber:session-archived", Some(session_id)) => vec![ServerEvent::SessionPatched {
            session_id,
            patch: SessionPatch {
                archived: Some(properties.get("archivedAt").and_then(Value::as_i64)),
                ..SessionPatch::default()
            },
        }],
        ("openchamber:session-metadata", Some(session_id)) => vec![ServerEvent::SessionPatched {
            session_id,
            patch: SessionPatch {
                metadata: Some(metadata_facts(properties.get("metadata"))),
                ..SessionPatch::default()
            },
        }],
        _ => vec![ServerEvent::Unsupported {
            kind: kind.to_owned(),
        }],
    };
    Some(events)
}

fn tool_text(content: Option<&Value>) -> String {
    let texts: Vec<&str> = content
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    texts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_ready_heartbeat_and_unknown_types() {
        let ready =
            decode_frame(r#"{"type":"ready","scope":"global","replayReset":true}"#).expect("valid");
        assert_eq!(ready.events, [ServerEvent::Ready { replay_reset: true }]);
        let heartbeat = decode_frame(
            r#"{"type":"event","payload":{"type":"openchamber:heartbeat","timestamp":1},"directory":"global"}"#,
        )
        .expect("valid");
        assert_eq!(heartbeat.events, [ServerEvent::Heartbeat]);
        let unknown = decode_frame(
            r#"{"type":"event","eventId":"evt_9","payload":{"id":"evt_9","type":"pty.created","data":{}}}"#,
        )
        .expect("valid");
        assert_eq!(unknown.event_id.as_deref(), Some("evt_9"));
        assert_eq!(
            unknown.events,
            [ServerEvent::Unsupported {
                kind: "pty.created".to_owned()
            }]
        );
        assert!(decode_frame("not json").is_err());
        assert!(decode_frame(r#"{"type":"error","message":"boom"}"#).is_err());
    }

    #[test]
    fn compactions_report_their_lifecycle() {
        let status = |frame: &str| match &decode_frame(frame).expect("valid").events[..] {
            [
                ServerEvent::Transcript {
                    event: TranscriptEvent::MessageUpdated(info),
                    ..
                },
            ] => (
                info.id.as_str().to_owned(),
                info.compaction,
                info.notice.clone(),
            ),
            other => panic!("one message update, not {other:?}"),
        };
        assert_eq!(
            status(
                r#"{"type":"event","payload":{"id":"evt_c1","created":5,"type":"session.compaction.started","data":{"sessionID":"s1","inputID":"msg_in","reason":"auto"}}}"#
            ),
            (
                "msg_in".to_owned(),
                Some(CompactionStatus::Running),
                Some(String::new())
            )
        );
        assert_eq!(
            status(
                r#"{"type":"event","payload":{"id":"evt_c2","created":6,"type":"session.compaction.ended","data":{"sessionID":"s1","reason":"auto","text":"summary"}}}"#
            ),
            (
                "msg_c2".to_owned(),
                Some(CompactionStatus::Completed),
                Some("summary".to_owned())
            )
        );
        assert_eq!(
            status(
                r#"{"type":"event","payload":{"id":"evt_c3","created":7,"type":"session.compaction.failed","data":{"sessionID":"s1","reason":"auto","error":{"type":"x","message":"y"}}}}"#
            ),
            (
                "msg_c3".to_owned(),
                Some(CompactionStatus::Failed),
                Some(String::new())
            )
        );
    }

    #[test]
    fn catalog_rebuilds_name_their_directory() {
        let scoped = decode_frame(
            r#"{"type":"event","payload":{"id":"evt_1","type":"agent.updated","location":{"directory":"/work/app"},"data":{}}}"#,
        )
        .expect("valid");
        assert_eq!(
            scoped.events,
            [ServerEvent::CatalogUpdated {
                kind: CatalogKind::Agent,
                directory: Some("/work/app".to_owned()),
            }]
        );
        let global = decode_frame(
            r#"{"type":"event","payload":{"id":"evt_2","type":"credential.switched","location":{"directory":"global"},"data":{}}}"#,
        )
        .expect("valid");
        assert_eq!(
            global.events,
            [ServerEvent::CatalogUpdated {
                kind: CatalogKind::Credential,
                directory: None,
            }]
        );
    }

    #[test]
    fn interrupted_by_shutdown_changes_nothing() {
        let frame = decode_frame(
            r#"{"type":"event","payload":{"id":"evt_1","created":5,"type":"session.execution.interrupted","data":{"sessionID":"s1","reason":"shutdown"}}}"#,
        )
        .expect("valid");
        assert!(frame.events.is_empty());
    }

    #[test]
    fn text_deltas_address_the_ordinal_part() {
        let frame = decode_frame(
            r#"{"type":"event","payload":{"id":"evt_1","created":5,"type":"session.text.delta","data":{"sessionID":"s1","assistantMessageID":"msg_a","ordinal":2,"delta":"hi"}}}"#,
        )
        .expect("valid");
        assert_eq!(
            frame.events,
            [ServerEvent::Transcript {
                session_id: SessionId::from("s1"),
                event: TranscriptEvent::PartDelta {
                    message_id: MessageId::from("msg_a"),
                    part_id: PartId::from("msg_a:text:2"),
                    delta: "hi".to_owned(),
                },
            }]
        );
    }
}
