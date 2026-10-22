//! Session records as the application model sees them.

use crate::ids::{OpenCodeProjectId, SessionId};
use crate::path;

/// What kind of conversation a session is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SessionKind {
    /// An ordinary conversation.
    #[default]
    Conversation,
    /// A temporary `/btw` side conversation forked from another session.
    ///
    /// These never appear in any sidebar projection and never promote their
    /// children into Chats.
    Btw,
}

/// OpenCode's project metadata attached to a session record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionProjectMetadata {
    pub id: Option<OpenCodeProjectId>,
    pub worktree: Option<String>,
}

/// Server timestamps in milliseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionTime {
    pub created: i64,
    pub updated: i64,
    /// When the last turn ended; absent until the session's first idle.
    pub idle: Option<i64>,
    /// Present when the session is archived.
    pub archived: Option<i64>,
}

/// OpenChamber's metadata overlay on a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionOverlay {
    /// `metadata.openchamber.pinned.at`, when the user pinned the session.
    pub pinned_at: Option<i64>,
    /// `metadata.openchamber.work.state == "open"`.
    pub in_work: bool,
}

/// A session record from a global or directory-scoped server read.
///
/// `directory` is the session's real directory. Display grouping never
/// rewrites it; routing, pin identity and drafts always use it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: SessionId,
    pub parent_id: Option<SessionId>,
    pub directory: Option<String>,
    pub title: String,
    pub time: SessionTime,
    /// `projectID` from the session payload.
    pub project_id: Option<OpenCodeProjectId>,
    /// `project` metadata from the session payload.
    pub project: Option<SessionProjectMetadata>,
    pub kind: SessionKind,
    pub overlay: SessionOverlay,
}

impl SessionRecord {
    /// Creates an active top-level conversation record.
    pub fn new(id: impl Into<SessionId>, directory: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            directory: Some(directory.into()),
            ..Self::default()
        }
    }

    pub fn is_archived(&self) -> bool {
        self.time.archived.is_some()
    }

    pub fn is_btw(&self) -> bool {
        self.kind == SessionKind::Btw
    }

    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }

    /// The normalized directory used for ownership resolution.
    ///
    /// Falls back to the project's worktree when the record has no usable
    /// directory.
    pub fn resolved_directory(&self) -> Option<String> {
        path::normalize_optional(self.directory.as_deref()).or_else(|| {
            self.project
                .as_ref()
                .and_then(|project| path::normalize_optional(project.worktree.as_deref()))
        })
    }

    /// `projectID`, falling back to `project.id`.
    pub fn opencode_project_id(&self) -> Option<&OpenCodeProjectId> {
        self.project_id
            .as_ref()
            .filter(|id| !id.as_str().is_empty())
            .or_else(|| {
                self.project
                    .as_ref()
                    .and_then(|project| project.id.as_ref())
            })
    }
}
