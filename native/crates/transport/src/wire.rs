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
    pub model: Option<AgentModel>,
    pub agent: Option<String>,
    pub revert: Option<Revert>,
}

/// `Session.Revert`: a staged revert's marker. Its file diffs are not read.
#[derive(Debug, Deserialize)]
pub(crate) struct Revert {
    #[serde(rename = "messageID")]
    pub message_id: String,
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
    pub source: Option<FileSource>,
    /// The bytes, base64, of a file the prompt carried inline.
    pub data: Option<String>,
}

/// Where a prompt's file came from (`PromptFileAttachment.source`).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum FileSource {
    Uri {
        uri: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AgentMention {
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct AssistantBody {
    #[serde(default)]
    pub content: Vec<AssistantItem>,
    pub agent: Option<String>,
    pub model: Option<AgentModel>,
    pub finish: Option<String>,
    /// `SessionStructuredError`; only its message is read, so a malformed
    /// error never fails the message.
    pub error: Option<Value>,
    pub tokens: Option<TokenUsage>,
    pub cost: Option<f64>,
}

/// `TokenUsageInfo`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct TokenUsage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
    #[serde(default)]
    pub cache: TokenCache,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TokenCache {
    #[serde(default)]
    pub read: u64,
    #[serde(default)]
    pub write: u64,
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
        time: Option<ItemTime>,
    },
    /// Boxed: a call's input and metadata make it the largest item.
    Tool(Box<ToolItem>),
    #[serde(other)]
    Unsupported,
}

/// A reasoning item's own times.
#[derive(Debug, Deserialize)]
pub(crate) struct ItemTime {
    pub created: Option<i64>,
    pub completed: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ToolItem {
    pub id: String,
    pub name: String,
    pub state: ToolState,
    pub time: Option<ToolTime>,
}

/// When a tool call was streamed, started running and finished.
#[derive(Debug, Deserialize)]
pub(crate) struct ToolTime {
    pub created: Option<i64>,
    pub ran: Option<i64>,
    pub completed: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub(crate) enum ToolState {
    Streaming,
    Running {
        input: Option<Value>,
        metadata: Option<Value>,
    },
    Completed {
        #[serde(default)]
        content: Vec<ToolContent>,
        input: Option<Value>,
        metadata: Option<Value>,
    },
    Error {
        error: StructuredError,
        #[serde(default)]
        content: Vec<ToolContent>,
        input: Option<Value>,
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

/// `GET /api/config/settings`: the fields topology and the composer's
/// defaults need. Values are checked one by one, as the web client's settings
/// parsers do: a malformed value is ignored, never the whole document.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Settings {
    pub auto_save_enabled: Option<Value>,
    pub terminal_close_on_exit: Option<Value>,
    pub terminal_shell: Option<Value>,
    #[serde(default)]
    pub projects: Vec<Value>,
    pub home_directory: Option<Value>,
    pub last_directory: Option<Value>,
    pub active_project_id: Option<Value>,
    pub default_model: Option<Value>,
    pub default_variant: Option<Value>,
    pub default_agent: Option<Value>,
    pub last_selected_model: Option<Value>,
    #[serde(default, deserialize_with = "effort_memory")]
    pub last_used_efforts: Option<Vec<(String, Value)>>,
    pub hidden_models: Option<Value>,
    pub routing_feature_available: Option<Value>,
    pub time_format_preference: Option<Value>,
    pub code_block_line_wrap: Option<Value>,
    pub message_search_enabled: Option<Value>,
    pub sidebar_grouping: Option<Value>,
    pub sidebar_view_mode: Option<Value>,
    pub sidebar_list_session_grouping: Option<Value>,
    pub sidebar_show_chats_section: Option<Value>,
    pub sidebar_show_pinned_section: Option<Value>,
    pub sidebar_show_work_section: Option<Value>,
    pub sidebar_keep_work_in_place: Option<Value>,
    pub session_work_enabled: Option<Value>,
    pub session_goal_enabled: Option<Value>,
    pub session_goal_checker: Option<Value>,
    pub session_goal_default_budget_enabled: Option<Value>,
    pub session_goal_default_budget: Option<Value>,
    pub prompt_navigator_enabled: Option<Value>,
    pub show_deletion_dialog: Option<Value>,
    pub permission_default_mode: Option<Value>,
    pub follow_up_behavior: Option<Value>,
    pub queue_mode_enabled: Option<Value>,
    pub draft_starters: Option<Value>,
    pub draft_starters_visible: Option<Value>,
    pub draft_starters_craft_goal_added: Option<Value>,
    pub draft_starters_schedule_task_added: Option<Value>,
}

// A JSON object's encounter order is the profile's effort-recency order.
// Decode this field directly, before serde_json::Value sorts its object keys.
fn effort_memory<'de, D>(deserializer: D) -> Result<Option<Vec<(String, Value)>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct MemoryVisitor;
    impl<'de> serde::de::Visitor<'de> for MemoryVisitor {
        type Value = Option<Vec<(String, Value)>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a model effort map")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut entries = Vec::new();
            while let Some(entry) = map.next_entry()? {
                entries.push(entry);
            }
            Ok(Some(entries))
        }

        fn visit_seq<S>(self, mut sequence: S) -> Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok(None)
        }
    }
    deserializer.deserialize_any(MemoryVisitor)
}

/// OpenCode v2 list routes: `{ location, data }`.
#[derive(Debug, Deserialize)]
pub(crate) struct Located<T> {
    pub data: T,
}

/// `GET /api/agent`.
#[derive(Debug, Deserialize)]
pub(crate) struct AgentInfo {
    pub id: String,
    pub name: Option<String>,
    pub mode: Option<String>,
    pub hidden: Option<bool>,
    pub options: Option<Value>,
    pub model: Option<AgentModel>,
    pub color: Option<String>,
}

/// A model reference: an agent's pinned model, or a session's current one.
#[derive(Debug, Deserialize)]
pub(crate) struct AgentModel {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub id: String,
    pub variant: Option<String>,
}

/// `GET /api/model`: one model of the flat catalog.
#[derive(Debug, Deserialize)]
pub(crate) struct ModelInfo {
    pub id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub name: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    pub variants: Option<Vec<ModelVariant>>,
    pub limit: Option<ModelLimit>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelLimit {
    pub context: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelVariant {
    pub id: String,
}

/// `GET /api/model/default`.
#[derive(Debug, Deserialize)]
pub(crate) struct DefaultModel {
    #[serde(rename = "modelID")]
    pub model_id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
}

/// `GET /api/provider`.
#[derive(Debug, Deserialize)]
pub(crate) struct ProviderInfo {
    pub id: String,
    pub name: Option<String>,
    pub activation: Option<String>,
}

/// `GET /api/config`: every discovered config document and folder, lowest
/// priority first.
#[derive(Debug, Deserialize)]
pub(crate) struct ConfigEntry {
    #[serde(rename = "type")]
    pub kind: String,
    pub info: Option<serde_json::Map<String, Value>>,
}

/// `GET /api/mcp`.
#[derive(Debug, Deserialize)]
pub(crate) struct McpServer {
    pub status: McpStatus,
}

#[derive(Debug, Deserialize)]
pub(crate) struct McpStatus {
    pub status: String,
}

/// `GET /api/config/skills`.
#[derive(Debug, Deserialize)]
pub(crate) struct Skills {
    #[serde(default)]
    pub skills: Vec<Value>,
}

/// `GET /api/sessions/status`: the host's cross-project status map.
#[derive(Debug, Deserialize)]
pub(crate) struct StatusSnapshot {
    pub sessions: std::collections::HashMap<String, StatusEntry>,
    #[serde(default)]
    pub pending: Option<serde_json::Value>,
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
