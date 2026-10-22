//! OpenCode 2.x and OpenChamber wire shapes, private to the transport.
//!
//! Only the fields the native client reads are declared; Serde ignores the
//! rest. Fields that are optional on the wire stay `Option`, so a missing
//! field and an empty one keep their distinct meanings.

use serde::Deserialize;
use serde_json::Value;

/// A cursor-paged list.
#[derive(Debug, Deserialize)]
pub(crate) struct Page<T> {
    pub data: Vec<T>,
    #[serde(default)]
    pub cursor: Cursor,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Cursor {
    pub next: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Location {
    pub directory: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SessionTime {
    pub created: i64,
    pub updated: i64,
    pub idle: Option<i64>,
    /// OpenChamber's archive overlay, folded in by the server.
    pub archived: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Session {
    pub id: String,
    #[serde(rename = "parentID")]
    pub parent_id: Option<String>,
    #[serde(rename = "projectID")]
    pub project_id: Option<String>,
    pub location: Location,
    pub title: Option<String>,
    pub time: SessionTime,
    pub metadata: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessageTime {
    pub created: i64,
    pub completed: Option<i64>,
}

/// A message record before its `type` is dispatched.
#[derive(Debug, Deserialize)]
pub(crate) struct MessageEnvelope {
    pub id: String,
    pub time: MessageTime,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub body: serde_json::Map<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct UserBody {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub files: Vec<FileAttachment>,
    #[serde(default)]
    pub agents: Vec<AgentMention>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FileAttachment {
    pub mime: String,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AgentMention {
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct AssistantBody {
    #[serde(default)]
    pub content: Vec<AssistantItem>,
}

/// One item of an assistant message's ordered content.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum AssistantItem {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Tool(ToolItem),
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ToolItem {
    pub id: String,
    pub name: String,
    pub state: ToolState,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub(crate) enum ToolState {
    Streaming,
    Running,
    Completed {
        #[serde(default)]
        content: Vec<ToolContent>,
        metadata: Option<Value>,
    },
    Error {
        error: StructuredError,
        #[serde(default)]
        content: Vec<ToolContent>,
        metadata: Option<Value>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum ToolContent {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StructuredError {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub message: String,
}

/// `GET /api/session/:id/message/:messageID`.
#[derive(Debug, Deserialize)]
pub(crate) struct Single<T> {
    pub data: T,
}

/// `/health`: the unauthenticated identity of an OpenChamber server.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Health {
    pub status: Option<String>,
    pub openchamber_version: Option<String>,
    pub runtime: Option<String>,
    pub server_id: Option<String>,
    pub compatibility: Option<Compatibility>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Compatibility {
    pub api_version: u32,
    pub min_client_api_version: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// `/api/fs/home`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HomeInfo {
    pub home: Option<String>,
    pub chats_root: Option<String>,
    pub canonical_chats_root: Option<String>,
    pub canonical_legacy_chats_root: Option<String>,
}

/// `GET /api/config/settings`: the fields topology needs.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Settings {
    /// Entries are checked one by one: a malformed entry is skipped, never
    /// the whole list.
    #[serde(default)]
    pub projects: Vec<Value>,
    pub home_directory: Option<String>,
}

/// `GET /api/sessions/status`: the host's cross-project status map.
#[derive(Debug, Deserialize)]
pub(crate) struct StatusSnapshot {
    pub sessions: std::collections::HashMap<String, StatusEntry>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StatusEntry {
    pub status: String,
}

/// `POST /api/git/project-worktrees`.
#[derive(Debug, Deserialize)]
pub(crate) struct WorktreeBatch {
    #[serde(default)]
    pub projects: Vec<WorktreeBatchEntry>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorktreeBatchEntry {
    pub directory: String,
    /// Absent or `null` when the server could not list this directory.
    pub worktrees: Option<Vec<WorktreeEntry>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorktreeEntry {
    pub path: Option<String>,
    pub branch: Option<String>,
    pub name: Option<String>,
}

/// `/auth/session` with `issueClientToken`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionLogin {
    pub client_token: Option<String>,
}
