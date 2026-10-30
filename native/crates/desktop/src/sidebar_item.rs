//! Owned snapshots of sidebar rows for rendering.
//!
//! The core row model borrows session records; GPUI entities cannot hold
//! borrows, so each rebuild copies the few fields a row paints.

use std::collections::HashMap;

use gpui_kit::SharedString;
use openchamber_native_core::grouping::GroupLabel;
use openchamber_native_core::ids::{ProjectId, SessionId};
use openchamber_native_core::sidebar_rows::{
    ActivityKey, EmptyKind, GroupLoadState, ShowControl, SidebarRow,
};

#[derive(Clone, Debug, PartialEq)]
pub enum SidebarItem {
    Draft {
        key: SharedString,
        draft: openchamber_native_core::draft::DraftKey,
        title: SharedString,
    },
    Zone {
        key: SharedString,
        activity: ActivityKey,
        collapsed: bool,
    },
    Project {
        key: SharedString,
        project_id: ProjectId,
        label: SharedString,
        collapsed: bool,
    },
    Group {
        key: SharedString,
        group_key: String,
        label: GroupLabel,
        collapsed: bool,
        empty: bool,
    },
    Worktree {
        key: SharedString,
        group_key: String,
        label: GroupLabel,
        expanded: bool,
        session_ids: Vec<SessionId>,
    },
    Session {
        key: SharedString,
        session_id: SessionId,
        title: SharedString,
        depth: usize,
        has_children: bool,
        expanded: bool,
        expansion_key: String,
        pinned: bool,
        /// The row is a work of its own, so it may offer Track and Done.
        owns_work: bool,
        /// The row lists an archived session.
        archived: bool,
    },
    Show {
        key: SharedString,
        control: ShowControl,
        container_key: String,
        current_count: usize,
        increment: usize,
        depth: usize,
        flat: bool,
    },
    Status {
        key: SharedString,
        state: GroupLoadState,
        depth: usize,
        flat: bool,
    },
    Empty {
        key: SharedString,
        kind: EmptyKind,
        depth: usize,
    },
}

impl SidebarItem {
    pub fn key(&self) -> &str {
        match self {
            Self::Draft { key, .. }
            | Self::Zone { key, .. }
            | Self::Project { key, .. }
            | Self::Group { key, .. }
            | Self::Worktree { key, .. }
            | Self::Session { key, .. }
            | Self::Show { key, .. }
            | Self::Status { key, .. }
            | Self::Empty { key, .. } => key,
        }
    }

    /// Copies a core row; project headers take the project folder's name.
    pub fn from_row(row: &SidebarRow<'_>, project_paths: &HashMap<&ProjectId, &str>) -> Self {
        match row {
            SidebarRow::ActivityHeader {
                key,
                activity,
                collapsed,
            } => Self::Zone {
                key: key.clone().into(),
                activity: *activity,
                collapsed: *collapsed,
            },
            SidebarRow::ProjectHeader {
                key,
                project_id,
                collapsed,
            } => Self::Project {
                key: key.clone().into(),
                project_id: project_id.clone(),
                label: project_label(project_paths, project_id),
                collapsed: *collapsed,
            },
            SidebarRow::GroupHeader {
                key,
                group_key,
                label,
                collapsed,
                empty,
                ..
            } => Self::Group {
                key: key.clone().into(),
                group_key: group_key.clone(),
                label: label.clone(),
                collapsed: *collapsed,
                empty: *empty,
            },
            SidebarRow::Worktree {
                key,
                group_key,
                label,
                expanded,
                session_ids,
                ..
            } => Self::Worktree {
                key: key.clone().into(),
                group_key: group_key.clone(),
                label: label.clone(),
                expanded: *expanded,
                session_ids: session_ids.clone(),
            },
            SidebarRow::Session {
                key,
                session,
                depth,
                has_children,
                expanded,
                expansion_key,
                owns_work,
                archived,
                ..
            } => Self::Session {
                key: key.clone().into(),
                session_id: session.id.clone(),
                title: session.title.clone().into(),
                depth: *depth,
                has_children: *has_children,
                expanded: *expanded,
                expansion_key: expansion_key.clone(),
                pinned: session.overlay.pinned_at.is_some(),
                owns_work: *owns_work,
                archived: *archived,
            },
            SidebarRow::ShowControl {
                key,
                control,
                container_key,
                current_count,
                increment,
                depth,
                flat,
            } => Self::Show {
                key: key.clone().into(),
                control: *control,
                container_key: container_key.clone(),
                current_count: *current_count,
                increment: *increment,
                depth: *depth,
                flat: *flat,
            },
            SidebarRow::Status {
                key,
                state,
                depth,
                flat,
                ..
            } => Self::Status {
                key: key.clone().into(),
                state: *state,
                depth: *depth,
                flat: *flat,
            },
            SidebarRow::Empty { key, kind, depth } => Self::Empty {
                key: key.clone().into(),
                kind: *kind,
                depth: *depth,
            },
        }
    }
}

/// A project's display name: the last segment of its folder.
fn project_label(
    project_paths: &HashMap<&ProjectId, &str>,
    project_id: &ProjectId,
) -> SharedString {
    let path = project_paths.get(project_id).copied().unwrap_or_default();
    path.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(path)
        .to_owned()
        .into()
}
