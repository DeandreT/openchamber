//! Frames of the server's global event bridge, translated into domain events.
//!
//! Mirrors `translateWireEvent` in `packages/ui/src/lib/opencode/events.ts`
//! and the OpenChamber frame handling in `sync/event-pipeline.ts` for the
//! session and transcript events the native client models. Every other type
//! becomes `ServerEvent::Unsupported` with its name only, never a fabricated
//! state change.

use openchamber_native_core::ids::{MessageId, OpenCodeProjectId, PartId, PermissionId, SessionId};
use openchamber_native_core::permission_mode::PolicySnapshot;
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
    /// `replay_through` names the last replayed event, absent when nothing
    /// was replayed.
    Ready {
        replay_reset: bool,
        replay_through: Option<String>,
    },
    SessionCreated(SessionRecord),
    SessionPatched {
        session_id: SessionId,
        patch: SessionPatch,
    },
    SessionDeleted {
        session_id: SessionId,
    },
    /// OpenCode forked `parent_id` into `session_id`. No `session.created`
    /// follows and the event carries no record, so the consumer reads it.
    SessionForked {
        session_id: SessionId,
        parent_id: SessionId,
    },
    /// A staged revert became permanent: OpenCode deleted `to` and every
    /// later message without per-message removals.
    RevertCommitted {
        session_id: SessionId,
        to: MessageId,
    },
    SessionStatus {
        session_id: SessionId,
        status: LiveStatus,
    },
    /// A turn boundary `SessionStatus` cannot express. It accompanies the
    /// status event of the same frame and never replaces it.
    SessionExecution {
        session_id: SessionId,
        edge: ExecutionEdge,
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
    /// The host's whole permission policy after a change.
    PermissionPolicy(PolicySnapshot),
    /// The server's auto-answer held a permission request back or failed:
    /// the request is the user's after all.
    PermissionLeftForUser {
        session_id: SessionId,
        permission_id: PermissionId,
        directory: Option<String>,
    },
    /// Routing availability changed; reread it rather than trust the frame.
    RoutingUpdated,
    /// One session's server-owned queue after a change. `space` names the
    /// isolated space whose own queue the host forwarded.
    MessageQueueUpdated {
        update: crate::message_queue::QueueUpdate,
        space: Option<String>,
    },
    /// One auto-review run after a change, from its owner.
    AutoReviewUpdated(crate::review::AutoReviewUpdate),
    Heartbeat,
    /// The server is shedding load; reconcile after it recovers.
    Backpressure,
    /// The server's per-session warming after a change.
    SessionWarming(crate::session_warming::WarmingSnapshot),
    /// Another client changed the shared session folders; `updated_at` is
    /// the stored revision.
    SessionFoldersChanged {
        updated_at: i64,
    },
    /// A notification the server raised for this user.
    Notification(crate::notification::UiNotification),
    /// A type this client does not model.
    Unsupported {
        kind: String,
    },
}

/// A turn boundary of one session's execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExecutionEdge {
    /// `session.execution.started`: the one event that marks where a turn
    /// begins. A busy status repeats at every step and marks nothing.
    Started,
    /// `session.execution.failed`: the turn ended with an error.
    Failed,
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
    replay_through: Option<String>,
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
            replay_through: frame.replay_through.filter(|id| !id.is_empty()),
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
            synthetic: None,
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
                revert: None,
                btw: facts.btw,
                goal: facts.goal,
                cost: None,
                context_pins: facts.context_pins,
            })]
        }
        "session.deleted" => vec![ServerEvent::SessionDeleted { session_id }],
        "session.forked" => match text("parentID").filter(|id| !id.is_empty()) {
            Some(parent) => vec![ServerEvent::SessionForked {
                session_id,
                parent_id: SessionId::from(parent),
            }],
            None => vec![ServerEvent::Unsupported {
                kind: kind.to_owned(),
            }],
        },
        "session.revert.staged" => match data
            .pointer("/revert/messageID")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            Some(marker) => vec![ServerEvent::SessionPatched {
                session_id,
                patch: SessionPatch {
                    revert: Some(Some(MessageId::from(marker))),
                    ..SessionPatch::default()
                },
            }],
            None => vec![ServerEvent::Unsupported {
                kind: kind.to_owned(),
            }],
        },
        "session.revert.cleared" => vec![ServerEvent::SessionPatched {
            session_id,
            patch: SessionPatch {
                revert: Some(None),
                ..SessionPatch::default()
            },
        }],
        // Trim first, then drop the marker, as `events.ts` orders them.
        "session.revert.committed" => match text("to").filter(|id| !id.is_empty()) {
            Some(to) => vec![
                ServerEvent::RevertCommitted {
                    session_id: session_id.clone(),
                    to: MessageId::from(to),
                },
                ServerEvent::SessionPatched {
                    session_id,
                    patch: SessionPatch {
                        revert: Some(None),
                        ..SessionPatch::default()
                    },
                },
            ],
            None => vec![ServerEvent::Unsupported {
                kind: kind.to_owned(),
            }],
        },
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
                cost: data
                    .get("cost")
                    .and_then(Value::as_f64)
                    .and_then(Usd::from_dollars),
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
        "session.synthetic" => vec![transcript(TranscriptEvent::MessageUpdated(MessageInfo {
            id: message_id_from_event(event_id),
            role: Role::Synthetic,
            created,
            completed: None,
            notice: text("text").map(str::to_owned),
            assistant: None,
            compaction: None,
            synthetic: Some(projection::synthetic_facts(
                text("description"),
                data.get("metadata"),
            )),
        }))],
        "session.skill.activated" => vec![notice(Role::Skill, text("name"))],
        "session.instructions.updated" => match text("text") {
            Some(instructions) => vec![notice(Role::System, Some(instructions))],
            None => Vec::new(),
        },
        "session.execution.started" => vec![
            ServerEvent::SessionExecution {
                session_id: session_id.clone(),
                edge: ExecutionEdge::Started,
            },
            status(LiveStatus::Busy),
        ],
        "session.execution.succeeded" => settled(SessionPatch {
            idle: Some(created),
            updated: Some(created),
            ..SessionPatch::default()
        }),
        // The failure precedes the idle status of the same frame, so no
        // consumer sees the turn as settled cleanly in between.
        "session.execution.failed" => {
            let mut events = vec![ServerEvent::SessionExecution {
                session_id: session_id.clone(),
                edge: ExecutionEdge::Failed,
            }];
            events.extend(settled(SessionPatch {
                idle: Some(created),
                updated: Some(created),
                ..SessionPatch::default()
            }));
            events
        }
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
                synthetic: None,
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
                    vec![transcript(TranscriptEvent::MessageUpdated(MessageInfo {
                        synthetic: Some(projection::synthetic_facts(
                            payload.get("description").and_then(Value::as_str),
                            payload.get("metadata"),
                        )),
                        ..info(Role::Synthetic, notice)
                    }))]
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
                synthetic: None,
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
                synthetic: None,
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
                        child: None,
                        facts: Box::default(),
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
            let mut failure = None;
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
                    failure = Some(error.clone()).filter(|error| !error.is_empty());
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
            let child = projection::tool_child(data.get("metadata"), data.get("input"));
            let facts =
                crate::tool_facts::tool_facts(data.get("input"), data.get("metadata"), failure);
            let mut events = vec![transcript(TranscriptEvent::ToolTransition {
                message_id: message_id.clone(),
                part_id: PartId::from(call_id),
                status,
                output,
                facts: Some(Box::new(facts)),
                at: created,
            })];
            if let Some(child) = child {
                events.push(transcript(TranscriptEvent::ToolChild {
                    message_id,
                    part_id: PartId::from(call_id),
                    child,
                }));
            }
            events
        }
        // A running subagent names its child only through progress.
        "session.tool.progress" => {
            match (
                assistant(),
                text("id"),
                projection::tool_child(data.get("metadata"), None),
            ) {
                (Some(message_id), Some(call_id), Some(child)) => {
                    vec![transcript(TranscriptEvent::ToolChild {
                        message_id,
                        part_id: PartId::from(call_id),
                        child,
                    })]
                }
                _ => Vec::new(),
            }
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
    if let Some(event) = permission_policy_event(kind, properties) {
        return Some(vec![event]);
    }
    if kind == "openchamber:session-warming" {
        return Some(vec![
            crate::session_warming::decode_snapshot(properties).map_or_else(
                || ServerEvent::Unsupported {
                    kind: kind.to_owned(),
                },
                ServerEvent::SessionWarming,
            ),
        ]);
    }
    if kind == "openchamber:session-folders-changed" {
        return Some(vec![
            properties
                .get("updatedAt")
                .and_then(Value::as_f64)
                .filter(|updated_at| updated_at.is_finite())
                .map_or_else(
                    || ServerEvent::Unsupported {
                        kind: kind.to_owned(),
                    },
                    |updated_at| ServerEvent::SessionFoldersChanged {
                        updated_at: updated_at as i64,
                    },
                ),
        ]);
    }
    if kind == "openchamber:message-queue.updated" {
        // A malformed broadcast changes nothing; the next read corrects it.
        return Some(vec![
            crate::message_queue::decode_update(properties).map_or_else(
                || ServerEvent::Unsupported {
                    kind: kind.to_owned(),
                },
                |(update, space)| ServerEvent::MessageQueueUpdated { update, space },
            ),
        ]);
    }
    if kind == "openchamber:notification" {
        return Some(vec![crate::notification::decode(properties).map_or_else(
            || ServerEvent::Unsupported {
                kind: kind.to_owned(),
            },
            ServerEvent::Notification,
        )]);
    }
    if kind == "openchamber:auto-review.updated" {
        // A malformed broadcast changes nothing; the next read corrects it.
        return Some(vec![crate::review::decode_update(properties).map_or_else(
            || ServerEvent::Unsupported {
                kind: kind.to_owned(),
            },
            ServerEvent::AutoReviewUpdated,
        )]);
    }
    let session_id = properties
        .get("sessionID")
        .or_else(|| properties.get("sessionId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(SessionId::from);
    let events = match (kind, session_id) {
        ("openchamber:heartbeat", _) => vec![ServerEvent::Heartbeat],
        ("openchamber:routing.updated", _) => vec![ServerEvent::RoutingUpdated],
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

/// The permission-mode broadcasts, as `event-pipeline.ts` validates them. A
/// policy or report the host forwarded from an isolated space (`spaceId`)
/// belongs to that space's own server, which this client does not read, so
/// it changes nothing here.
fn permission_policy_event(kind: &str, properties: &Value) -> Option<ServerEvent> {
    let unsupported = || ServerEvent::Unsupported {
        kind: kind.to_owned(),
    };
    match kind {
        "openchamber:permission-auto-accept.updated" => {
            if properties.get("spaceId").is_some() {
                return Some(unsupported());
            }
            Some(
                crate::permission_policy::decode(properties.clone())
                    .map_or_else(|_| unsupported(), ServerEvent::PermissionPolicy),
            )
        }
        "openchamber:permission-auto-accept.left-for-user" => {
            let text = |field: &str| {
                properties
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
            };
            let directory = match properties.get("directory") {
                Some(Value::String(directory)) => Some(directory.clone()),
                Some(Value::Null) => None,
                _ => return Some(unsupported()),
            };
            match (text("sessionId"), text("permissionId")) {
                (Some(session), Some(permission)) if properties.get("spaceId").is_none() => {
                    Some(ServerEvent::PermissionLeftForUser {
                        session_id: SessionId::from(session),
                        permission_id: PermissionId::from(permission),
                        directory,
                    })
                }
                _ => Some(unsupported()),
            }
        }
        _ => None,
    }
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
    fn decodes_notification_broadcasts() {
        let frame = decode_frame(
            r#"{"type":"event","payload":{"type":"openchamber:notification","properties":{"kind":"completion","sessionId":"ses_1","directory":"/repo","title":"Ready","body":"Done","requireHidden":true,"desktopNotificationDelivered":false,"desktopStdoutActive":false}}}"#,
        )
        .expect("valid");
        let [ServerEvent::Notification(notification)] = frame.events.as_slice() else {
            panic!("a notification: {:?}", frame.events);
        };
        assert_eq!(notification.title.as_deref(), Some("Ready"));
        assert_eq!(notification.directory.as_deref(), Some("/repo"));
        let malformed = decode_frame(
            r#"{"type":"event","payload":{"type":"openchamber:notification","properties":{"title":1}}}"#,
        )
        .expect("valid frame");
        assert!(matches!(
            malformed.events.as_slice(),
            [ServerEvent::Unsupported { .. }]
        ));
    }

    #[test]
    fn decodes_message_queue_broadcasts() {
        let frame = decode_frame(
            r#"{"type":"event","payload":{"type":"openchamber:message-queue.updated","properties":{"revision":3,"session":{"sessionId":"ses_1","directory":"/repo","items":[],"sendingId":"q1"}}}}"#,
        )
        .expect("valid");
        let [ServerEvent::MessageQueueUpdated { update, space }] = frame.events.as_slice() else {
            panic!("a queue update: {:?}", frame.events);
        };
        assert_eq!(update.revision, 3);
        assert_eq!(
            update.session.sending.as_ref().map(|id| id.as_str()),
            Some("q1")
        );
        assert_eq!(space, &None);
        let malformed = decode_frame(
            r#"{"type":"event","payload":{"type":"openchamber:message-queue.updated","properties":{"revision":"x"}}}"#,
        )
        .expect("valid frame");
        assert!(matches!(
            malformed.events.as_slice(),
            [ServerEvent::Unsupported { .. }]
        ));
    }

    #[test]
    fn decodes_ready_heartbeat_and_unknown_types() {
        let ready =
            decode_frame(r#"{"type":"ready","scope":"global","replayReset":true}"#).expect("valid");
        assert_eq!(
            ready.events,
            [ServerEvent::Ready {
                replay_reset: true,
                replay_through: None
            }]
        );
        let through = decode_frame(r#"{"type":"ready","scope":"global","replayThrough":"e9"}"#)
            .expect("valid");
        assert_eq!(
            through.events,
            [ServerEvent::Ready {
                replay_reset: false,
                replay_through: Some("e9".to_owned())
            }]
        );
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
    fn permission_policy_broadcasts_decode_and_space_policies_are_left_out() {
        use openchamber_native_core::permission_mode::PermissionMode;
        let frame = |properties: &str, kind: &str| {
            decode_frame(&format!(
                r#"{{"type":"event","payload":{{"type":"openchamber:permission-auto-accept.{kind}","properties":{properties}}}}}"#
            ))
            .expect("valid frame")
            .events
        };
        assert_eq!(
            frame(
                r#"{"sessions":{"s":true},"modes":{"s":"safety"},"revision":3}"#,
                "updated"
            ),
            [ServerEvent::PermissionPolicy(PolicySnapshot {
                modes: [(SessionId::from("s"), PermissionMode::Safety)].into(),
                revision: Some(3),
            })]
        );
        assert_eq!(
            frame(
                r#"{"permissionId":"per_1","sessionId":"s","directory":"/repo"}"#,
                "left-for-user"
            ),
            [ServerEvent::PermissionLeftForUser {
                session_id: SessionId::from("s"),
                permission_id: PermissionId::from("per_1"),
                directory: Some("/repo".to_owned()),
            }]
        );
        for (properties, kind) in [
            (
                r#"{"sessions":{"s":true},"spaceId":"0123456789ab"}"#,
                "updated",
            ),
            (r#"{"sessions":{"s":"on"}}"#, "updated"),
            (r#"{"sessionId":"s","directory":"/repo"}"#, "left-for-user"),
            (
                r#"{"permissionId":"per_1","sessionId":"s"}"#,
                "left-for-user",
            ),
            (
                r#"{"permissionId":"per_1","sessionId":"s","directory":null,"spaceId":"0123456789ab"}"#,
                "left-for-user",
            ),
        ] {
            assert!(
                matches!(
                    &frame(properties, kind)[..],
                    [ServerEvent::Unsupported { .. }]
                ),
                "{properties} changes nothing"
            );
        }
    }

    fn events(payload: &str) -> Vec<ServerEvent> {
        decode_frame(&format!(r#"{{"type":"event","payload":{payload}}}"#))
            .expect("valid frame")
            .events
    }

    #[test]
    fn forks_and_reverts_reach_the_session_and_its_transcript() {
        assert_eq!(
            events(
                r#"{"id":"evt_1","created":5,"type":"session.forked","data":{"sessionID":"ses_f","parentID":"ses_p","boundary":{}}}"#
            ),
            [ServerEvent::SessionForked {
                session_id: SessionId::from("ses_f"),
                parent_id: SessionId::from("ses_p"),
            }]
        );
        let patch = |revert: Option<Option<&str>>| SessionPatch {
            revert: revert.map(|marker| marker.map(MessageId::from)),
            ..SessionPatch::default()
        };
        assert_eq!(
            events(
                r#"{"id":"evt_2","created":5,"type":"session.revert.staged","data":{"sessionID":"s","revert":{"messageID":"msg_2","files":[]}}}"#
            ),
            [ServerEvent::SessionPatched {
                session_id: SessionId::from("s"),
                patch: patch(Some(Some("msg_2"))),
            }]
        );
        assert_eq!(
            events(
                r#"{"id":"evt_3","created":5,"type":"session.revert.cleared","data":{"sessionID":"s"}}"#
            ),
            [ServerEvent::SessionPatched {
                session_id: SessionId::from("s"),
                patch: patch(Some(None)),
            }]
        );
        assert_eq!(
            events(
                r#"{"id":"evt_4","created":5,"type":"session.revert.committed","data":{"sessionID":"s","to":"msg_2"}}"#
            ),
            [
                ServerEvent::RevertCommitted {
                    session_id: SessionId::from("s"),
                    to: MessageId::from("msg_2"),
                },
                ServerEvent::SessionPatched {
                    session_id: SessionId::from("s"),
                    patch: patch(Some(None)),
                },
            ]
        );
        assert!(matches!(
            &events(
                r#"{"id":"evt_5","type":"session.revert.staged","data":{"sessionID":"s","revert":{}}}"#
            )[..],
            [ServerEvent::Unsupported { .. }]
        ));
    }

    #[test]
    fn synthetic_records_keep_their_subagent_run() {
        let [
            ServerEvent::Transcript {
                event: TranscriptEvent::MessageUpdated(info),
                ..
            },
        ] = &events(
            r#"{"id":"evt_9","created":5,"type":"session.synthetic","data":{"sessionID":"s","text":"<subagent>done</subagent>","description":"Explore","metadata":{"source":"subagent","childID":"ses_c","agent":"explore","state":"completed"}}}"#,
        )[..]
        else {
            panic!("one synthetic record");
        };
        assert_eq!(info.id.as_str(), "msg_9");
        let facts = info.synthetic.as_ref().expect("synthetic facts");
        assert_eq!(facts.description.as_deref(), Some("Explore"));
        let run = facts.subagent.as_ref().expect("a subagent run");
        assert_eq!(
            (run.child.as_str(), run.agent.as_deref(), run.state),
            (
                "ses_c",
                Some("explore"),
                openchamber_native_core::transcript::SubagentRunState::Completed
            )
        );
        let [
            ServerEvent::Transcript {
                event: TranscriptEvent::MessageUpdated(plain),
                ..
            },
        ] = &events(
            r#"{"id":"evt_10","created":5,"type":"session.synthetic","data":{"sessionID":"s","text":"ctx","metadata":{"source":"subagent","state":"done"}}}"#,
        )[..]
        else {
            panic!("one synthetic record");
        };
        assert_eq!(
            plain
                .synthetic
                .as_ref()
                .and_then(|facts| facts.subagent.as_ref()),
            None,
            "an invalid run is ordinary context"
        );
    }

    #[test]
    fn subagent_calls_name_their_child() {
        let child = |payload: &str| -> Vec<String> {
            events(payload)
                .into_iter()
                .filter_map(|event| match event {
                    ServerEvent::Transcript {
                        event: TranscriptEvent::ToolChild { child, .. },
                        ..
                    } => Some(child.as_str().to_owned()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            child(
                r#"{"id":"e","created":5,"type":"session.tool.progress","data":{"sessionID":"s","assistantMessageID":"msg_a","id":"call_1","metadata":{"sessionID":"ses_c"}}}"#
            ),
            ["ses_c"]
        );
        assert_eq!(
            child(
                r#"{"id":"e","created":5,"type":"session.tool.success","data":{"sessionID":"s","assistantMessageID":"msg_a","id":"call_1","content":[{"type":"text","text":"ok"}],"metadata":{"sessionId":"ses_d"},"executed":true}}"#
            ),
            ["ses_d"]
        );
        assert!(
            child(
                r#"{"id":"e","created":5,"type":"session.tool.called","data":{"sessionID":"s","assistantMessageID":"msg_a","id":"call_1","input":{},"executed":true}}"#
            )
            .is_empty()
        );
    }

    #[test]
    fn execution_edges_accompany_their_status_events() {
        let decode = |kind: &str| {
            decode_frame(&format!(
                r#"{{"type":"event","payload":{{"id":"evt_1","created":5,"type":"{kind}","data":{{"sessionID":"s1"}}}}}}"#
            ))
            .expect("valid")
            .events
        };
        let session_id = SessionId::from("s1");
        let status = |status: LiveStatus| ServerEvent::SessionStatus {
            session_id: session_id.clone(),
            status,
        };
        let edge = |edge: ExecutionEdge| ServerEvent::SessionExecution {
            session_id: session_id.clone(),
            edge,
        };
        let settled = ServerEvent::SessionPatched {
            session_id: session_id.clone(),
            patch: SessionPatch {
                idle: Some(5),
                updated: Some(5),
                ..SessionPatch::default()
            },
        };
        assert_eq!(
            decode("session.execution.started"),
            [edge(ExecutionEdge::Started), status(LiveStatus::Busy)]
        );
        assert_eq!(
            decode("session.execution.failed"),
            [
                edge(ExecutionEdge::Failed),
                settled.clone(),
                status(LiveStatus::Idle)
            ]
        );
        assert_eq!(
            decode("session.execution.succeeded"),
            [settled, status(LiveStatus::Idle)],
            "a clean end carries no edge"
        );
        assert!(
            decode("session.status").is_empty(),
            "a status without a type marks nothing"
        );
    }

    #[test]
    fn usage_updates_carry_the_session_cost() {
        let decode = |cost: &str| {
            decode_frame(&format!(
                r#"{{"type":"event","payload":{{"id":"evt_1","created":5,"type":"session.usage.updated","data":{{"sessionID":"s1"{cost}}}}}}}"#
            ))
            .expect("valid")
            .events
        };
        let patched = |cost: Option<Usd>| {
            [ServerEvent::SessionPatched {
                session_id: SessionId::from("s1"),
                patch: SessionPatch {
                    updated: Some(5),
                    cost,
                    ..SessionPatch::default()
                },
            }]
        };
        assert_eq!(decode(r#","cost":0.25"#), patched(Usd::from_dollars(0.25)));
        assert_eq!(decode(r#","cost":0"#), patched(Usd::from_dollars(0.0)));
        assert_eq!(
            decode(""),
            patched(None),
            "a missing cost keeps the known one"
        );
        assert_eq!(decode(r#","cost":"1""#), patched(None));
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
