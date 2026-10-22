//! Frames of the server's global event bridge, translated into domain events.
//!
//! Mirrors `translateWireEvent` in `packages/ui/src/lib/opencode/events.ts`
//! and the OpenChamber frame handling in `sync/event-pipeline.ts` for the
//! session and transcript events the native client models. Every other type
//! becomes `ServerEvent::Unsupported` with its name only, never a fabricated
//! state change.

use openchamber_native_core::ids::{MessageId, PartId, SessionId};
use openchamber_native_core::session::{SessionKind, SessionOverlay, SessionRecord, SessionTime};
use openchamber_native_core::transcript::{
    MessageInfo, Part, PartContent, Role, ToolStatus, TranscriptEvent,
};
use serde::Deserialize;
use serde_json::Value;

use crate::error::TransportError;
use crate::projection::{overlay, part_ids};

/// Live run state of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LiveStatus {
    Busy,
    Idle,
    Retry,
}

/// A change to one session record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionPatch {
    pub title: Option<String>,
    pub updated: Option<i64>,
    pub idle: Option<i64>,
    /// `Some(None)` un-archives; `None` leaves the archive state alone.
    pub archived: Option<Option<i64>>,
    pub directory: Option<String>,
    /// Replaces the OpenChamber overlay read from the full metadata.
    pub overlay: Option<(SessionOverlay, SessionKind)>,
}

/// One domain change announced by the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerEvent {
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
    Heartbeat,
    /// The server is shedding load; reconcile after it recovers.
    Backpressure,
    /// A type this client does not model.
    Unsupported {
        kind: String,
    },
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
            Some(payload) => translate(&payload),
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
        }))
    };
    let assistant = || text("assistantMessageID").map(MessageId::from);
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
        "session.created" => vec![ServerEvent::SessionCreated(SessionRecord {
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
            kind: overlay(data.get("metadata")).1,
            overlay: overlay(data.get("metadata")).0,
        })],
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
                overlay: Some(overlay(data.get("metadata"))),
                ..SessionPatch::default()
            },
        }],
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
            if data.pointer("/item/type").and_then(Value::as_str) != Some("user") {
                return Vec::new();
            }
            let message_id = MessageId::from(inbox_id);
            let prompt = data
                .pointer("/item/payload/text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut events = vec![transcript(TranscriptEvent::MessageUpdated(MessageInfo {
                id: message_id.clone(),
                role: Role::User,
                created,
                completed: None,
                notice: None,
            }))];
            if !prompt.is_empty() {
                events.push(transcript(TranscriptEvent::PartUpdated {
                    message_id,
                    part: Part::text(PartId::new(part_ids::text(inbox_id, 0)), prompt),
                }));
            }
            events
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
            }))],
            None => Vec::new(),
        },
        "session.step.ended" | "session.step.failed" => match assistant() {
            Some(message_id) => vec![transcript(TranscriptEvent::MessageCompleted {
                message_id,
                completed: created,
            })],
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
            vec![transcript(TranscriptEvent::PartUpdated {
                message_id,
                part: Part {
                    id: PartId::new(id),
                    content,
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
                Some("retry") => LiveStatus::Retry,
                Some("idle") => LiveStatus::Idle,
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
                overlay: Some(overlay(properties.get("metadata"))),
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
