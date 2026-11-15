//! Permission requests and explicit replies. Saved rules remain server-owned.

use crate::ids::{MessageId, PermissionId, SessionId};
use std::fmt;

/// The tool call that raised a permission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionSource {
    pub message_id: MessageId,
    pub call_id: String,
}

/// Authoritative permission fields; metadata is display-only and cannot grant authority.
#[derive(Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub id: PermissionId,
    pub session_id: SessionId,
    pub action: String,
    pub resources: Vec<String>,
    pub save: Vec<String>,
    pub source: Option<PermissionSource>,
    pub message: Option<String>,
    pub metadata_display: Option<String>,
}
impl fmt::Debug for PermissionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermissionRequest")
            .field("id", &self.id)
            .field("session_id", &self.session_id)
            .field("action", &self.action)
            .field("source", &self.source)
            .field("resources", &"[redacted]")
            .field("save", &"[redacted]")
            .field("message", &"[redacted]")
            .field("metadata_display", &"[redacted]")
            .finish()
    }
}

impl PermissionRequest {
    /// Whether an explicit Always reply would save any rules.
    pub fn can_save(&self) -> bool {
        !self.save.is_empty()
    }
}

/// An explicit user decision, never inferred from settings or request content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionReply {
    Once,
    Always,
    Reject,
}

impl PermissionReply {
    /// The OpenCode 2.2 decision spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Always => "always",
            Self::Reject => "reject",
        }
    }
}
