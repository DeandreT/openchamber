//! Replays the official client's recorded requests and projections.
//!
//! `tests/fixtures/transport.json` comes from
//! `packages/ui/src/lib/native/transportFixtures.ts`
//! (`bun run native:transport-fixtures`); its own test fails when stale.

use std::collections::BTreeMap;

use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::session::{LiveStatus, SessionKind, SessionRecord};
use openchamber_native_core::session_index::SessionIndex;
use openchamber_native_core::transcript::{
    Message, PartContent, Role, ToolStatus, Transcript, TranscriptChange,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{MessagePage, ServerClient, encode_directory, message_page};
use crate::credential::Credential;
use crate::endpoint::Endpoint;
use crate::event::{self, ServerEvent};
use crate::identity::{PART_SHELLS_CAPABILITY, ServerIdentity};
use crate::{projection, wire};

const FIXTURE: &str = include_str!("../tests/fixtures/transport.json");
const SESSIONS: &str = include_str!("../tests/fixtures/wire/sessions.json");
const MESSAGES_RECORDED: &str = include_str!("../tests/fixtures/wire/messages-recorded.json");
const MESSAGES_LONG: &str = include_str!("../tests/fixtures/wire/messages-long.json");
const MESSAGES_TOOLS: &str = include_str!("../tests/fixtures/wire/messages-tools.json");
const MESSAGES_TOOLS_SHELLED: &str =
    include_str!("../tests/fixtures/wire/messages-tools-shelled.json");
const MESSAGE_TOOLS_SINGLE: &str = include_str!("../tests/fixtures/wire/message-tools-single.json");
const FRAMES_RECORDED: &str = include_str!("../tests/fixtures/wire/frames-recorded.json");
const FRAMES_TOOLS: &str = include_str!("../tests/fixtures/wire/frames-tools.json");
const RECORDED_SESSION: &str = "ses_e9b49c443ffefXcWWPhCOga6nH";
const TOOLS_SESSION: &str = "ses_e9b3bfe12ffeMuiZLLg25eNcms";
const TOOLS_MESSAGE: &str = "msg_164c422cf001JfI01Q6WJWl0lr";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    base_url: String,
    scoped_directory: String,
    requests: Vec<RecordedRequest>,
    sessions: Vec<Value>,
    next_cursor: Option<String>,
    messages: BTreeMap<String, MessageFixture>,
    whole_message: Value,
    session_events: BTreeMap<String, Vec<Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordedRequest {
    name: String,
    method: String,
    path: String,
    query: Vec<(String, String)>,
    directory_header: Option<String>,
    part_shells_header: Option<String>,
}

#[derive(Deserialize)]
struct MessageFixture {
    limit: usize,
    /// Newest first, as the server lists them.
    items: Vec<Value>,
    older: Option<String>,
}

fn fixture() -> Fixture {
    serde_json::from_str(FIXTURE).expect("the transport fixture is valid JSON")
}

fn request<'fixture>(fixture: &'fixture Fixture, name: &str) -> &'fixture RecordedRequest {
    fixture
        .requests
        .iter()
        .find(|request| request.name == name)
        .unwrap_or_else(|| panic!("the fixture records the {name} request"))
}

fn sorted_query(url: &reqwest::Url) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    pairs.sort();
    pairs
}

fn normalize_session(session: &SessionRecord) -> Value {
    json!({
        "id": session.id.as_str(),
        "parentId": session.parent_id.as_ref().map(SessionId::as_str),
        "directory": session.directory,
        "title": session.title,
        "created": session.time.created,
        "updated": session.time.updated,
        "idle": session.time.idle,
        "archived": session.time.archived,
        "pinned": session.overlay.pinned_at.is_some(),
        "inWork": session.overlay.in_work,
        "btw": session.is_btw(),
    })
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Synthetic => "synthetic",
        Role::System => "system",
        Role::Skill => "skill",
        Role::Shell => "shell",
        Role::Compaction => "compaction",
        Role::AgentSwitched => "agent-switched",
        Role::ModelSwitched => "model-switched",
        Role::LocationSwitched => "location-switched",
        Role::Idle => "idle",
    }
}

fn normalize_message(message: &Message) -> Value {
    let parts: Vec<Value> = message
        .parts
        .iter()
        .map(|part| match &part.content {
            PartContent::Text { text } => {
                json!({"id": part.id.as_str(), "type": "text", "text": text})
            }
            PartContent::Reasoning { text } => {
                json!({"id": part.id.as_str(), "type": "reasoning", "text": text})
            }
            PartContent::Tool {
                tool,
                status,
                output,
                elided,
                ..
            } => json!({
                "id": part.id.as_str(),
                "type": "tool",
                "tool": tool,
                "status": match status {
                    ToolStatus::Pending => "pending",
                    ToolStatus::Running => "running",
                    ToolStatus::Completed => "completed",
                    ToolStatus::Error => "error",
                },
                "output": output,
                "elided": elided,
            }),
            PartContent::File { filename, mime } => {
                json!({"id": part.id.as_str(), "type": "file", "filename": filename, "mime": mime})
            }
            PartContent::Agent { name } => {
                json!({"id": part.id.as_str(), "type": "agent", "name": name})
            }
            PartContent::Unsupported { kind } => json!({"id": part.id.as_str(), "type": kind}),
        })
        .collect();
    json!({
        "id": message.info.id.as_str(),
        "role": role_name(message.info.role),
        "created": message.info.created,
        "completed": message.info.completed,
        "parts": parts,
    })
}

/// A recorded page as the client returns it: oldest first.
fn project_page(raw: &str, limit: usize) -> MessagePage {
    let page: wire::Page<wire::MessageEnvelope> =
        serde_json::from_str(raw).expect("the recorded page is valid");
    message_page(page, limit).expect("recorded messages parse")
}

/// Ids and parts only: live events stamp event times, history its own.
fn without_times(message: &Message) -> Value {
    let mut value = normalize_message(message);
    value["created"] = Value::Null;
    value["completed"] = Value::Null;
    value
}

#[test]
fn requests_match_the_official_client() {
    let fixture = fixture();
    let client = ServerClient::new(
        Endpoint::parse(&fixture.base_url).expect("the fixture base URL is valid"),
        Credential::None,
    )
    .expect("a client builds");

    let first = request(&fixture, "global sessions page");
    let url = client.session_page_url(None);
    assert_eq!(first.method, "GET");
    assert_eq!(url.path(), first.path);
    assert_eq!(sorted_query(&url), first.query);
    assert_eq!(
        first.directory_header, None,
        "global reads name no directory"
    );

    let next = request(&fixture, "global sessions next page");
    let url = client.session_page_url(Some("CURSOR_FROM_PREVIOUS_PAGE"));
    assert_eq!(url.path(), next.path);
    assert_eq!(sorted_query(&url), next.query);

    let messages = request(&fixture, "latest messages");
    let url = client.messages_url(&SessionId::from(RECORDED_SESSION), 50);
    assert_eq!(url.path(), messages.path);
    assert_eq!(sorted_query(&url), messages.query);
    assert_eq!(
        messages.directory_header, None,
        "session routes resolve their own location"
    );

    let shelled = request(&fixture, "latest messages, shelled");
    let identity = ServerIdentity {
        openchamber_version: "2.4.0".to_owned(),
        runtime: "web".to_owned(),
        server_id: None,
        api_version: 1,
        capabilities: vec![PART_SHELLS_CAPABILITY.to_owned()],
    };
    let shells = client.clone().with_identity(&identity);
    let built = shells
        .latest_messages_request(&SessionId::from(TOOLS_SESSION), 50)
        .build()
        .expect("the request builds");
    assert_eq!(built.url().path(), shelled.path);
    assert_eq!(sorted_query(built.url()), shelled.query);
    assert_eq!(
        built
            .headers()
            .get("x-openchamber-part-shells")
            .and_then(|value| value.to_str().ok()),
        shelled.part_shells_header.as_deref()
    );
    assert!(built.headers().get("x-opencode-directory").is_none());
    let plain = client
        .latest_messages_request(&SessionId::from(TOOLS_SESSION), 50)
        .build()
        .expect("the request builds");
    assert!(
        plain.headers().get("x-openchamber-part-shells").is_none(),
        "a server that does not announce shells is never asked for them"
    );

    let whole = request(&fixture, "whole message");
    let url = client.session_message_url(
        &SessionId::from(TOOLS_SESSION),
        &MessageId::from(TOOLS_MESSAGE),
    );
    assert_eq!(url.path(), whole.path);
    assert_eq!(sorted_query(&url), whole.query);
    assert_eq!(
        whole.part_shells_header, None,
        "a whole message is never shelled"
    );

    let scoped = request(&fixture, "directory sessions page");
    assert_eq!(
        Some(encode_directory(&fixture.scoped_directory)),
        scoped.directory_header,
        "directory headers are encoded as encodeURIComponent does"
    );
}

#[test]
fn sessions_project_as_the_ui_does() {
    let fixture = fixture();
    let page: wire::Page<wire::Session> =
        serde_json::from_str(SESSIONS).expect("the recorded page is valid");
    assert_eq!(page.cursor.next, fixture.next_cursor);
    let projected: Vec<Value> = page
        .data
        .into_iter()
        .map(projection::session)
        .map(|session| normalize_session(&session))
        .collect();
    assert_eq!(projected, fixture.sessions);
}

#[test]
fn messages_project_as_the_ui_does() {
    let fixture = fixture();
    for (name, raw) in [
        ("recorded", MESSAGES_RECORDED),
        ("long", MESSAGES_LONG),
        ("tools", MESSAGES_TOOLS),
        ("shelled", MESSAGES_TOOLS_SHELLED),
    ] {
        let expected = &fixture.messages[name];
        let page = project_page(raw, expected.limit);
        let projected: Vec<Value> = page.messages.iter().rev().map(normalize_message).collect();
        assert_eq!(projected, expected.items, "the {name} page diverged");
        assert_eq!(
            page.older, expected.older,
            "the {name} page's older cursor diverged"
        );
    }

    let single: wire::Single<wire::MessageEnvelope> =
        serde_json::from_str(MESSAGE_TOOLS_SINGLE).expect("the recorded message is valid");
    let whole = projection::message(single.data)
        .expect("the recorded message parses")
        .expect("assistant messages are kept");
    assert_eq!(normalize_message(&whole), fixture.whole_message);
}

/// A shelled page replaces nothing a transcript holds whole, and the whole
/// message completes a shelled call.
#[test]
fn shells_never_replace_whole_results() {
    let whole = project_page(MESSAGES_TOOLS, 50).messages;
    let shelled = project_page(MESSAGES_TOOLS_SHELLED, 50).messages;
    assert_ne!(whole, shelled, "the shelled page leaves results out");

    let mut transcript = Transcript::from_history(whole.clone());
    let _ = transcript.replace(shelled.clone());
    assert_eq!(transcript.messages(), whole.as_slice());

    let mut transcript = Transcript::from_history(shelled);
    let single: wire::Single<wire::MessageEnvelope> =
        serde_json::from_str(MESSAGE_TOOLS_SINGLE).expect("valid");
    let completed = projection::message(single.data)
        .expect("parses")
        .expect("kept");
    let row = transcript
        .row_of(&completed.info.id)
        .expect("the page holds the message");
    let _ = transcript.push(completed.clone());
    assert_eq!(transcript.message(row), Some(&completed));
}

/// Replaying the live stream rebuilds the transcript the history read returns.
///
/// Idle records exist only in history: the live stream reports the end of a
/// turn as a status change.
#[test]
fn replayed_events_converge_on_the_history() {
    for (session, frames, history) in [
        (RECORDED_SESSION, FRAMES_RECORDED, MESSAGES_RECORDED),
        (TOOLS_SESSION, FRAMES_TOOLS, MESSAGES_TOOLS),
    ] {
        let frames: Vec<Value> = serde_json::from_str(frames).expect("valid");
        let session = SessionId::from(session);
        let mut transcript = Transcript::new();
        for frame in frames {
            let decoded = event::decode_frame(&frame.to_string()).expect("recorded frames decode");
            for event in decoded.events {
                if let ServerEvent::Transcript { session_id, event } = event
                    && session_id == session
                {
                    let change = transcript.apply(event);
                    assert_ne!(
                        change,
                        TranscriptChange::NeedsReconcile,
                        "a recorded event of {session} refers to a message the stream announced"
                    );
                }
            }
        }
        let replayed: Vec<Value> = transcript.messages().iter().map(without_times).collect();
        let expected: Vec<Value> = project_page(history, 50)
            .messages
            .iter()
            .filter(|message| message.info.role != Role::Idle)
            .map(without_times)
            .collect();
        assert!(
            !expected.is_empty(),
            "the history of {session} has messages"
        );
        assert_eq!(replayed, expected, "the replay of {session} diverged");
    }
}

/// The session-level effects of one event, normalized as the fixture
/// normalizes the UI's sync events.
fn session_effect(event: &ServerEvent) -> Option<Value> {
    match event {
        ServerEvent::SessionCreated(record) => {
            Some(json!({"type": "created", "session": normalize_session(record)}))
        }
        ServerEvent::SessionPatched { session_id, patch } => {
            let mut value = json!({"type": "patched", "sessionId": session_id.as_str()});
            let fields = [
                ("title", patch.title.as_ref().map(|title| json!(title))),
                (
                    "directory",
                    patch.directory.as_ref().map(|directory| json!(directory)),
                ),
                (
                    "projectId",
                    patch.project_id.as_ref().map(|id| json!(id.as_str())),
                ),
                ("updated", patch.updated.map(|time| json!(time))),
                ("idle", patch.idle.map(|time| json!(time))),
                ("archived", patch.archived.map(|time| json!(time))),
                (
                    "overlay",
                    patch.metadata.as_ref().map(|facts| {
                        json!({
                            "pinned": facts.overlay.pinned_at.is_some(),
                            "inWork": facts.overlay.in_work,
                            "btw": facts.kind == SessionKind::Btw,
                        })
                    }),
                ),
            ];
            for (key, field) in fields {
                if let Some(field) = field {
                    value[key] = field;
                }
            }
            Some(value)
        }
        ServerEvent::SessionDeleted { session_id } => {
            Some(json!({"type": "deleted", "sessionId": session_id.as_str()}))
        }
        ServerEvent::SessionStatus { session_id, status } => Some(json!({
            "type": "status",
            "sessionId": session_id.as_str(),
            "status": match status {
                LiveStatus::Busy => "busy",
                LiveStatus::Idle => "idle",
                LiveStatus::Retry => "retry",
            },
        })),
        _ => None,
    }
}

/// Session records, patches, removals and live status follow the UI's
/// classification of the same frames, in the same order.
#[test]
fn session_events_match_the_ui() {
    let fixture = fixture();
    for (name, frames) in [("recorded", FRAMES_RECORDED), ("tools", FRAMES_TOOLS)] {
        let frames: Vec<Value> = serde_json::from_str(frames).expect("valid");
        let mut effects = Vec::new();
        for frame in frames {
            let decoded = event::decode_frame(&frame.to_string()).expect("recorded frames decode");
            effects.extend(decoded.events.iter().filter_map(session_effect));
        }
        assert_eq!(
            effects, fixture.session_events[name],
            "the {name} session events diverged"
        );
    }
}

/// Replaying a recorded stream into the index rebuilds the session the
/// server lists afterwards.
#[test]
fn replayed_session_events_converge_on_the_listing() {
    let page: wire::Page<wire::Session> = serde_json::from_str(SESSIONS).expect("valid");
    let listed = page
        .data
        .into_iter()
        .map(projection::session)
        .find(|session| session.id.as_str() == RECORDED_SESSION)
        .expect("the listing holds the recorded session");
    let mut index = SessionIndex::new();
    let ticket = index.begin_load();
    let _ = index.complete_load(ticket, Vec::new());
    let frames: Vec<Value> = serde_json::from_str(FRAMES_RECORDED).expect("valid");
    for frame in frames {
        let decoded = event::decode_frame(&frame.to_string()).expect("recorded frames decode");
        let _ = index.apply(
            decoded
                .events
                .iter()
                .filter_map(ServerEvent::session_change),
        );
    }
    let replayed = index
        .get(&SessionId::from(RECORDED_SESSION))
        .expect("the stream created the session");
    assert!(
        replayed.same_structure(&listed),
        "replayed {replayed:?} and listed {listed:?} differ"
    );
    // Recency differs by design: events stamp their own time, as the UI does.
    assert_eq!(replayed.time.idle, listed.time.idle);
}
