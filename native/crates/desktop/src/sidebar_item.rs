//! Owned snapshots of sidebar rows for rendering.
//!
//! The core row model borrows session records; GPUI entities cannot hold
//! borrows, so each rebuild copies the few fields a row paints.

use std::collections::HashMap;

use gpui_kit::SharedString;
use openchamber_native_core::grouping::GroupLabel;
use openchamber_native_core::ids::{ProjectId, SessionId};
use openchamber_native_core::sidebar_rows::{
    ActivityKey, EmptyKind, GroupLoadState, RenderContext, ShowControl, SidebarRow,
};

#[derive(Clone, Debug, PartialEq)]
pub enum SidebarItem {
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
    Session {
        key: SharedString,
        session_id: SessionId,
        title: SharedString,
        depth: usize,
        has_children: bool,
        expanded: bool,
        expansion_key: String,
        pinned: bool,
        context: RenderContext,
    },
    Show {
        key: SharedString,
        control: ShowControl,
        container_key: String,
        current_count: usize,
        increment: usize,
        depth: usize,
    },
    Status {
        key: SharedString,
        state: GroupLoadState,
        depth: usize,
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
            Self::Zone { key, .. }
            | Self::Project { key, .. }
            | Self::Group { key, .. }
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
            } => {
                let path = project_paths.get(project_id).copied().unwrap_or_default();
                let label = path
                    .rsplit('/')
                    .find(|segment| !segment.is_empty())
                    .unwrap_or(path);
                Self::Project {
                    key: key.clone().into(),
                    project_id: project_id.clone(),
                    label: label.to_owned().into(),
                    collapsed: *collapsed,
                }
            }
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
            SidebarRow::Session {
                key,
                session,
                depth,
                has_children,
                expanded,
                expansion_key,
                context,
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
                context: *context,
            },
            SidebarRow::ShowControl {
                key,
                control,
                container_key,
                current_count,
                increment,
                depth,
            } => Self::Show {
                key: key.clone().into(),
                control: *control,
                container_key: container_key.clone(),
                current_count: *current_count,
                increment: *increment,
                depth: *depth,
            },
            SidebarRow::Status {
                key, state, depth, ..
            } => Self::Status {
                key: key.clone().into(),
                state: *state,
                depth: *depth,
            },
            SidebarRow::Empty { key, kind, depth } => Self::Empty {
                key: key.clone().into(),
                kind: *kind,
                depth: *depth,
            },
        }
    }
}
