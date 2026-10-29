//! Session records as the application model sees them.

use crate::ids::{OpenCodeProjectId, SessionId};
use crate::path;
use crate::send_config::ModelChoice;

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
    /// An independent review identified by explicit OpenChamber metadata.
    Review,
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
    /// The model the session's next turn uses, as the server last reported it.
    pub model: Option<ModelChoice>,
    /// The agent the session's next turn uses, as the server last reported it.
    pub agent: Option<String>,
}

/// What a session's full metadata establishes for the client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetadataFacts {
    pub overlay: SessionOverlay,
    pub kind: SessionKind,
}

/// A change to fields of one session record, as live events report it.
///
/// Absent fields stay as they are.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionPatch {
    pub title: Option<String>,
    pub directory: Option<String>,
    pub project_id: Option<OpenCodeProjectId>,
    pub updated: Option<i64>,
    pub idle: Option<i64>,
    /// `Some(None)` restores an archived session.
    pub archived: Option<Option<i64>>,
    /// Replaces what the previous full metadata established.
    pub metadata: Option<MetadataFacts>,
    /// The newly selected model.
    pub model: Option<ModelChoice>,
    /// The newly selected agent.
    pub agent: Option<String>,
}

impl SessionPatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Live run state of a session. Only the live channel reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LiveStatus {
    Busy,
    Idle,
    Retry,
}

impl SessionRecord {
    /// Electron's prompt policy. Review metadata takes precedence over a stale parent ID.
    pub fn prompt_read_only(
        &self,
        allow_prompting_subagents: bool,
        surface_read_only: bool,
    ) -> bool {
        if self.kind == SessionKind::Review {
            surface_read_only
        } else if self
            .parent_id
            .as_ref()
            .is_some_and(|parent| !parent.as_str().is_empty())
        {
            !allow_prompting_subagents
        } else {
            surface_read_only
        }
    }

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

    /// This record with `patch` applied, as `applyPatch` in
    /// `packages/ui/src/sync/session-event-router.ts` applies it.
    pub fn patched(&self, patch: &SessionPatch) -> Self {
        let mut next = self.clone();
        if let Some(title) = &patch.title {
            next.title.clone_from(title);
        }
        if let Some(directory) = &patch.directory {
            next.directory = Some(directory.clone());
        }
        if let Some(project_id) = &patch.project_id {
            next.project_id = Some(project_id.clone());
        }
        if let Some(updated) = patch.updated {
            next.time.updated = updated;
        }
        if let Some(idle) = patch.idle {
            next.time.idle = Some(idle);
        }
        if let Some(archived) = patch.archived {
            next.time.archived = archived;
        }
        if let Some(facts) = patch.metadata {
            next.overlay = facts.overlay;
            next.kind = facts.kind;
        }
        if let Some(model) = &patch.model {
            next.model = Some(model.clone());
        }
        if let Some(agent) = &patch.agent {
            next.agent = Some(agent.clone());
        }
        next
    }

    /// The time the record last changed: `updated`, else `created`.
    pub fn recency(&self) -> i64 {
        if self.time.updated != 0 {
            self.time.updated
        } else {
            self.time.created
        }
    }

    /// Whether `other` differs from this record only in recency, so the
    /// sidebar's structure is the same for both.
    pub fn same_structure(&self, other: &Self) -> bool {
        self.id == other.id
            && self.title == other.title
            && self.parent_id == other.parent_id
            && self.time.created == other.time.created
            && self.time.archived == other.time.archived
            && self.overlay == other.overlay
            && self.kind == other.kind
            && self.provider_id() == other.provider_id()
            && self.resolved_directory() == other.resolved_directory()
    }

    /// The provider of the session's model, whose logo its sidebar row shows.
    pub fn provider_id(&self) -> Option<&str> {
        self.model
            .as_ref()
            .map(|model| model.provider_id.as_str())
            .filter(|id| !id.is_empty())
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
