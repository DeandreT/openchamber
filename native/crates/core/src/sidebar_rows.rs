//! The ordered, mode-neutral sidebar row projection.
//!
//! Mirrors the normal-mode Tree view of `buildSessionSidebarRowModel` in
//! `packages/ui/src/components/session/sidebar/sessionSidebarRowModel.ts`:
//! Chats, Pinned and In work zones above the projects, project and worktree
//! headers, session trees with explicit expansion, reveal controls, status
//! notices and the empty state. Every row has a stable key so the renderer
//! can retain row state across rebuilds.

use std::collections::{HashMap, HashSet};

use crate::grouping::{GroupLabel, ProjectSection, SessionGroup, SessionNode};
use crate::ids::{ProjectId, SessionId};
use crate::ordering::SessionOrdering;
use crate::session::SessionRecord;

const CHATS_INITIAL_LIMIT: usize = 3;
const CHATS_INCREMENT: usize = 7;
const SESSIONS_INITIAL_LIMIT: usize = 7;
const SESSIONS_INCREMENT: usize = 7;

/// A zone above the projects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActivityKey {
    Chats,
    Pinned,
    Work,
}

impl ActivityKey {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chats => "chats",
            Self::Pinned => "pinned",
            Self::Work => "work",
        }
    }
}

/// How a session row is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RenderContext {
    /// A one-line tree row under a project.
    Project,
    /// A one-line Pinned or In work row; sessions of every project mix.
    MixedCompact,
    /// A one-line Chats row in the Tree view.
    TreeChat,
}

impl RenderContext {
    fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::MixedCompact => "mixed-compact",
            Self::TreeChat => "tree-chat",
        }
    }

    /// Zone rows never expand.
    fn expands(self) -> bool {
        self != Self::MixedCompact
    }
}

/// Load state of a group's directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GroupLoadState {
    Ready,
    Loading,
    LoadFailed,
    InitializationFailed,
    PermissionDenied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShowControl {
    More,
    Fewer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EmptyKind {
    Sidebar,
    Archived,
}

/// One sidebar row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarRow<'a> {
    ActivityHeader {
        key: String,
        activity: ActivityKey,
        collapsed: bool,
    },
    ProjectHeader {
        key: String,
        project_id: ProjectId,
        collapsed: bool,
    },
    GroupHeader {
        key: String,
        group_key: String,
        project_id: Option<ProjectId>,
        label: GroupLabel,
        directory: Option<String>,
        collapsed: bool,
        /// Nothing to list, so the header offers no chevron.
        empty: bool,
    },
    Session {
        key: String,
        session: &'a SessionRecord,
        depth: usize,
        has_children: bool,
        expanded: bool,
        /// The key toggling this row's expansion.
        expansion_key: String,
        project_id: Option<ProjectId>,
        archived: bool,
        context: RenderContext,
    },
    ShowControl {
        key: String,
        control: ShowControl,
        container_key: String,
        current_count: usize,
        increment: usize,
        depth: usize,
    },
    Status {
        key: String,
        group_key: String,
        state: GroupLoadState,
        depth: usize,
    },
    Empty {
        key: String,
        kind: EmptyKind,
        depth: usize,
    },
}

impl SidebarRow<'_> {
    pub fn key(&self) -> &str {
        match self {
            Self::ActivityHeader { key, .. }
            | Self::ProjectHeader { key, .. }
            | Self::GroupHeader { key, .. }
            | Self::Session { key, .. }
            | Self::ShowControl { key, .. }
            | Self::Status { key, .. }
            | Self::Empty { key, .. } => key,
        }
    }
}

/// Client-owned presentation state of the sidebar.
///
/// Presentation never establishes server ownership or activity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SidebarState {
    pub collapsed_projects: HashSet<ProjectId>,
    pub collapsed_groups: HashSet<String>,
    pub collapsed_activities: HashSet<ActivityKey>,
    pub expanded_parents: HashSet<String>,
    pub visible_count_by_container: HashMap<String, usize>,
}

impl SidebarState {
    /// Toggles a parent row's expansion by its expansion key.
    pub fn toggle_expanded(&mut self, expansion_key: &str) {
        if !self.expanded_parents.remove(expansion_key) {
            self.expanded_parents.insert(expansion_key.to_owned());
        }
    }

    /// Applies a reveal control.
    pub fn reveal(
        &mut self,
        container_key: &str,
        control: ShowControl,
        current: usize,
        increment: usize,
    ) {
        match control {
            ShowControl::More => {
                self.visible_count_by_container
                    .insert(container_key.to_owned(), current + increment);
            }
            ShowControl::Fewer => {
                self.visible_count_by_container.remove(container_key);
            }
        }
    }
}

/// A session listed in the Pinned or In work zone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZoneItem<'a> {
    pub node: SessionNode<'a>,
    pub project_id: Option<ProjectId>,
}

/// Everything the row model reads.
#[derive(Clone, Copy, Debug)]
pub struct SidebarInputs<'v, 'a> {
    pub chat_group: Option<&'v SessionGroup<'a>>,
    pub pinned: &'v [ZoneItem<'a>],
    pub work: &'v [ZoneItem<'a>],
    pub sections: &'v [ProjectSection<'a>],
    pub state: &'v SidebarState,
    pub group_status: &'v HashMap<String, GroupLoadState>,
    /// Show only the active project's primary workspace.
    pub show_only_main_workspace: bool,
    pub active_project_id: Option<&'v ProjectId>,
}

/// The built rows and their index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SidebarRowModel<'a> {
    pub rows: Vec<SidebarRow<'a>>,
    pub row_index_by_key: HashMap<String, usize>,
    /// Header rows that stick while their section scrolls, in row order.
    pub sticky_headers: Vec<usize>,
    /// Session rows in display order, for keyboard navigation.
    pub session_order: Vec<SessionId>,
}

impl<'a> SidebarRowModel<'a> {
    /// The row showing `session_id` first, if any.
    pub fn session_row(&self, session_id: &SessionId) -> Option<usize> {
        self.rows.iter().position(
            |row| matches!(row, SidebarRow::Session { session, .. } if &session.id == session_id),
        )
    }

    /// The sticky header in effect when `first_visible` is the top row.
    pub fn sticky_header_for(&self, first_visible: usize) -> Option<usize> {
        self.sticky_headers
            .iter()
            .copied()
            .take_while(|index| *index <= first_visible)
            .last()
    }
}

/// Builds the sidebar rows for the normal Tree view.
pub fn build<'a>(
    inputs: &SidebarInputs<'_, 'a>,
    ordering: &mut SessionOrdering,
) -> SidebarRowModel<'a> {
    let mut builder = RowBuilder {
        inputs,
        ordering,
        model: SidebarRowModel::default(),
        key_occurrences: HashMap::new(),
        pinned_in_zone: inputs
            .pinned
            .iter()
            .map(|item| &item.node.session.id)
            .collect(),
    };
    builder.append_chats();
    builder.append_zone(ActivityKey::Pinned, inputs.pinned);
    builder.append_zone(ActivityKey::Work, inputs.work);
    builder.append_projects();
    if builder.model.rows.is_empty() {
        builder.push(SidebarRow::Empty {
            key: "sidebar:empty".to_owned(),
            kind: EmptyKind::Sidebar,
            depth: 0,
        });
    }
    let mut model = builder.model;
    model.row_index_by_key = model
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| (row.key().to_owned(), index))
        .collect();
    model
}

struct GroupLimits {
    initial: usize,
    increment: usize,
    pinned_always_visible: bool,
    context: RenderContext,
}

struct AppendOptions<'k> {
    container_key: &'k str,
    project_id: Option<&'k ProjectId>,
    archived: bool,
    context: RenderContext,
    base_depth: usize,
}

struct RowBuilder<'i, 'v, 'a, 'o> {
    inputs: &'i SidebarInputs<'v, 'a>,
    ordering: &'o mut SessionOrdering,
    model: SidebarRowModel<'a>,
    key_occurrences: HashMap<String, usize>,
    pinned_in_zone: HashSet<&'v SessionId>,
}

impl<'i, 'v, 'a, 'o> RowBuilder<'i, 'v, 'a, 'o> {
    fn push(&mut self, row: SidebarRow<'a>) {
        self.model.rows.push(row);
    }

    /// A key unique within the model; repeats get an occurrence suffix.
    fn unique_key(&mut self, base: String) -> String {
        let occurrence = self.key_occurrences.entry(base.clone()).or_insert(0);
        let key = if *occurrence == 0 {
            base
        } else {
            format!("{base}:{occurrence}")
        };
        *occurrence += 1;
        key
    }

    fn append_activity_header(&mut self, activity: ActivityKey) -> bool {
        let collapsed = self.inputs.state.collapsed_activities.contains(&activity);
        self.model.sticky_headers.push(self.model.rows.len());
        self.push(SidebarRow::ActivityHeader {
            key: format!("activity:{}:header", activity.as_str()),
            activity,
            collapsed,
        });
        collapsed
    }

    fn append_chats(&mut self) {
        let Some(group) = self.inputs.chat_group else {
            return;
        };
        if self.append_activity_header(ActivityKey::Chats) {
            return;
        }
        self.append_group(
            group,
            "activity:chats",
            None,
            true,
            Some(GroupLimits {
                initial: CHATS_INITIAL_LIMIT,
                increment: CHATS_INCREMENT,
                pinned_always_visible: true,
                context: RenderContext::TreeChat,
            }),
        );
    }

    fn append_zone(&mut self, activity: ActivityKey, items: &'v [ZoneItem<'a>]) {
        if items.is_empty() {
            return;
        }
        if self.append_activity_header(activity) {
            return;
        }
        let container_key = format!("activity:{}", activity.as_str());
        for item in items {
            self.append_trees(
                std::slice::from_ref(&item.node),
                &AppendOptions {
                    container_key: &container_key,
                    project_id: item.project_id.as_ref(),
                    archived: false,
                    context: RenderContext::MixedCompact,
                    base_depth: 0,
                },
            );
        }
    }

    fn append_projects(&mut self) {
        let inputs = self.inputs;
        let sections: Vec<&ProjectSection<'a>> = if inputs.show_only_main_workspace {
            let active = inputs
                .sections
                .iter()
                .find(|section| Some(&section.project.id) == inputs.active_project_id)
                .or_else(|| inputs.sections.first());
            active.into_iter().collect()
        } else {
            inputs.sections.iter().collect()
        };
        for section in sections {
            let project_id = &section.project.id;
            let collapsed = !inputs.show_only_main_workspace
                && inputs.state.collapsed_projects.contains(project_id);
            if !inputs.show_only_main_workspace {
                self.model.sticky_headers.push(self.model.rows.len());
                self.push(SidebarRow::ProjectHeader {
                    key: format!("project:{project_id}:header"),
                    project_id: project_id.clone(),
                    collapsed,
                });
            }
            if collapsed {
                continue;
            }
            for (group, hide_header) in group_descriptors(section, inputs.show_only_main_workspace)
            {
                let group_key = format!("{project_id}:{}", group.id);
                self.append_group(group, &group_key, Some(project_id), hide_header, None);
            }
        }
    }

    fn append_group(
        &mut self,
        group: &'v SessionGroup<'a>,
        group_key: &str,
        project_id: Option<&'v ProjectId>,
        hide_header: bool,
        limits: Option<GroupLimits>,
    ) {
        let mut source: Vec<&'v SessionNode<'a>> = group
            .sessions
            .iter()
            .filter(|node| !self.pinned_in_zone.contains(&node.session.id))
            .collect();
        let ordering = &mut *self.ordering;
        source.sort_by(|left, right| {
            ordering.compare(
                left.session,
                right.session,
                is_pinned(left.session),
                is_pinned(right.session),
            )
        });

        let stored_collapsed = self.inputs.state.collapsed_groups.contains(group_key);
        let content_depth = usize::from(!hide_header);
        let status = self.inputs.group_status.get(group_key).copied();
        let loaded = status.is_none_or(|state| state == GroupLoadState::Ready);
        let empty = source.is_empty() && loaded;
        if !hide_header {
            self.push(SidebarRow::GroupHeader {
                key: format!("{group_key}:header"),
                group_key: group_key.to_owned(),
                project_id: project_id.cloned(),
                label: group.label.clone(),
                directory: group.directory.clone(),
                collapsed: stored_collapsed && !empty,
                empty,
            });
        }
        // An empty group is always open, so its first session shows at once.
        if stored_collapsed && !empty {
            return;
        }

        let context = limits
            .as_ref()
            .map_or(RenderContext::Project, |limits| limits.context);
        let initial = limits
            .as_ref()
            .map_or(SESSIONS_INITIAL_LIMIT, |limits| limits.initial);
        let increment = limits
            .as_ref()
            .map_or(SESSIONS_INCREMENT, |limits| limits.increment);
        let pinned_always_visible = limits
            .as_ref()
            .is_some_and(|limits| limits.pinned_always_visible);
        let requested = self
            .inputs
            .state
            .visible_count_by_container
            .get(group_key)
            .copied()
            .unwrap_or(initial)
            .max(initial);
        // Pinned sessions stay visible whatever the reveal limit is and do not
        // spend it.
        let is_always_visible =
            |node: &SessionNode<'_>| pinned_always_visible && is_pinned(node.session);
        let limited_count = source
            .iter()
            .filter(|node| !is_always_visible(node))
            .count();
        let mut budget = requested;
        let visible: Vec<&'v SessionNode<'a>> = if group.is_archived_bucket {
            source.clone()
        } else {
            source
                .iter()
                .copied()
                .filter(|node| {
                    if is_always_visible(node) {
                        return true;
                    }
                    if budget == 0 {
                        return false;
                    }
                    budget -= 1;
                    true
                })
                .collect()
        };
        let visible_nodes: Vec<SessionNode<'a>> =
            visible.iter().map(|node| (*node).clone()).collect();
        self.append_trees(
            &visible_nodes,
            &AppendOptions {
                container_key: group_key,
                project_id,
                archived: group.is_archived_bucket,
                context,
                base_depth: content_depth,
            },
        );
        let remaining = source.len() - visible.len();
        let limited_visible = requested - budget;
        if !group.is_archived_bucket && remaining > 0 {
            self.push(SidebarRow::ShowControl {
                key: format!("{group_key}:more"),
                control: ShowControl::More,
                container_key: group_key.to_owned(),
                current_count: limited_visible,
                increment,
                depth: content_depth,
            });
        } else if !group.is_archived_bucket && limited_count > initial && remaining == 0 {
            self.push(SidebarRow::ShowControl {
                key: format!("{group_key}:fewer"),
                control: ShowControl::Fewer,
                container_key: group_key.to_owned(),
                current_count: limited_visible,
                increment,
                depth: content_depth,
            });
        }

        match status {
            Some(state) if source.is_empty() && state != GroupLoadState::Ready => {
                self.push_status(group_key, state, content_depth);
            }
            None | Some(GroupLoadState::Ready) if source.is_empty() && group.is_archived_bucket => {
                self.push(SidebarRow::Empty {
                    key: format!("{group_key}:empty"),
                    kind: EmptyKind::Archived,
                    depth: content_depth,
                });
            }
            Some(state)
                if !source.is_empty()
                    && state != GroupLoadState::Ready
                    && state != GroupLoadState::Loading =>
            {
                self.push_status(group_key, state, content_depth);
            }
            _ => {}
        }
    }

    fn push_status(&mut self, group_key: &str, state: GroupLoadState, depth: usize) {
        self.push(SidebarRow::Status {
            key: format!("{group_key}:status"),
            group_key: group_key.to_owned(),
            state,
            depth,
        });
    }

    fn append_trees(&mut self, nodes: &[SessionNode<'a>], options: &AppendOptions<'_>) {
        let mut stack: Vec<(&SessionNode<'a>, usize)> = nodes
            .iter()
            .rev()
            .map(|node| (node, options.base_depth))
            .collect();
        while let Some((node, depth)) = stack.pop() {
            let session = node.session;
            let key = self.unique_key(format!("{}:session:{}", options.container_key, session.id));
            let expansion_key = format!(
                "{}:{}:{}",
                options.context.as_str(),
                if options.archived {
                    "archived"
                } else {
                    "active"
                },
                session.id
            );
            let has_children = options.context.expands() && !node.children.is_empty();
            let expanded =
                has_children && self.inputs.state.expanded_parents.contains(&expansion_key);
            self.model.session_order.push(session.id.clone());
            self.push(SidebarRow::Session {
                key,
                session,
                depth,
                has_children,
                expanded,
                expansion_key,
                project_id: options.project_id.cloned(),
                archived: options.archived,
                context: options.context,
            });
            if !expanded {
                continue;
            }
            for child in node.children.iter().rev() {
                stack.push((child, depth + 1));
            }
        }
    }
}

fn is_pinned(session: &SessionRecord) -> bool {
    session.overlay.pinned_at.is_some()
}

/// The groups a project shows, each with whether its header is hidden.
///
/// The main group has no header; its sessions sit right under the project.
fn group_descriptors<'s, 'a>(
    section: &'s ProjectSection<'a>,
    main_workspace_only: bool,
) -> Vec<(&'s SessionGroup<'a>, bool)> {
    let groups = &section.groups;
    let primary = groups
        .iter()
        .find(|group| group.is_main && !group.sessions.is_empty())
        .or_else(|| groups.iter().find(|group| !group.sessions.is_empty()))
        .or_else(|| groups.iter().find(|group| group.is_main))
        .or_else(|| groups.first());
    let Some(primary) = primary else {
        return Vec::new();
    };
    if main_workspace_only {
        let archived = groups
            .iter()
            .find(|group| group.is_archived_bucket && group.id != primary.id);
        let mut descriptors = vec![(primary, true)];
        descriptors.extend(archived.map(|group| (group, false)));
        return descriptors;
    }
    let main = groups.iter().filter(|group| group.is_main).take(1);
    let others = groups.iter().filter(|group| !group.is_main);
    main.chain(others)
        .map(|group| (group, group.is_main))
        .collect()
}

/// Builds Pinned and In work items from sessions in lifecycle order.
///
/// Pinned lists active pinned root sessions; In work lists active root
/// sessions whose work is open. Each item keeps its own subtree.
pub fn zone_items<'a>(
    sections: &[ProjectSection<'a>],
    chat_group: Option<&SessionGroup<'a>>,
    ordering: &mut SessionOrdering,
) -> (Vec<ZoneItem<'a>>, Vec<ZoneItem<'a>>) {
    let mut pinned: Vec<ZoneItem<'a>> = Vec::new();
    let mut work: Vec<ZoneItem<'a>> = Vec::new();
    let mut collect = |group: &SessionGroup<'a>, project_id: Option<&ProjectId>| {
        if group.is_archived_bucket {
            return;
        }
        for node in &group.sessions {
            let item = || ZoneItem {
                node: node.clone(),
                project_id: project_id.cloned(),
            };
            if is_pinned(node.session) {
                pinned.push(item());
            }
            if node.session.overlay.in_work {
                work.push(item());
            }
        }
    };
    if let Some(group) = chat_group {
        collect(group, None);
    }
    for section in sections {
        for group in &section.groups {
            collect(group, Some(&section.project.id));
        }
    }
    for items in [&mut pinned, &mut work] {
        items.sort_by(|left, right| {
            ordering.compare(
                left.node.session,
                right.node.session,
                is_pinned(left.node.session),
                is_pinned(right.node.session),
            )
        });
    }
    (pinned, work)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grouping::DraftTarget;
    use crate::ownership::ConfiguredProject;

    fn node(session: &SessionRecord) -> SessionNode<'_> {
        SessionNode {
            session,
            children: Vec::new(),
        }
    }

    fn group<'a>(id: &str, is_main: bool, sessions: Vec<SessionNode<'a>>) -> SessionGroup<'a> {
        SessionGroup {
            id: id.to_owned(),
            label: GroupLabel::ProjectRoot { branch: None },
            directory: Some("/p".to_owned()),
            is_main,
            is_archived_bucket: false,
            space: None,
            draft_target: DraftTarget::Project,
            sessions,
        }
    }

    fn sessions(count: usize) -> Vec<SessionRecord> {
        (0..count)
            .map(|index| {
                let mut session = SessionRecord::new(format!("s{index:02}"), "/p");
                session.time.updated = 100 - index as i64;
                session
            })
            .collect()
    }

    fn kinds(model: &SidebarRowModel<'_>) -> Vec<String> {
        model
            .rows
            .iter()
            .map(|row| match row {
                SidebarRow::ActivityHeader { activity, .. } => {
                    format!("zone:{}", activity.as_str())
                }
                SidebarRow::ProjectHeader { project_id, .. } => format!("project:{project_id}"),
                SidebarRow::GroupHeader { group_key, .. } => format!("group:{group_key}"),
                SidebarRow::Session { session, depth, .. } => format!("{}@{depth}", session.id),
                SidebarRow::ShowControl { control, .. } => format!("show:{control:?}"),
                SidebarRow::Status { state, .. } => format!("status:{state:?}"),
                SidebarRow::Empty { kind, .. } => format!("empty:{kind:?}"),
            })
            .collect()
    }

    #[test]
    fn an_empty_sidebar_shows_the_empty_row() {
        let state = SidebarState::default();
        let status = HashMap::new();
        let model = build(
            &SidebarInputs {
                chat_group: None,
                pinned: &[],
                work: &[],
                sections: &[],
                state: &state,
                group_status: &status,
                show_only_main_workspace: false,
                active_project_id: None,
            },
            &mut SessionOrdering::new(),
        );
        assert_eq!(kinds(&model), ["empty:Sidebar"]);
    }

    #[test]
    fn projects_reveal_seven_sessions_then_offer_more_and_fewer() {
        let records = sessions(9);
        let project = ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/p".to_owned(),
        };
        let sections = vec![ProjectSection {
            project: &project,
            groups: vec![group("root", true, records.iter().map(node).collect())],
        }];
        let mut state = SidebarState::default();
        let status = HashMap::new();
        let inputs = |state: &SidebarState| -> Vec<String> {
            let model = build(
                &SidebarInputs {
                    chat_group: None,
                    pinned: &[],
                    work: &[],
                    sections: &sections,
                    state,
                    group_status: &status,
                    show_only_main_workspace: false,
                    active_project_id: None,
                },
                &mut SessionOrdering::new(),
            );
            kinds(&model)
        };
        let collapsed = inputs(&state);
        assert_eq!(collapsed.len(), 1 + 7 + 1);
        assert_eq!(collapsed.last().map(String::as_str), Some("show:More"));
        state.reveal("app:root", ShowControl::More, 7, 7);
        let revealed = inputs(&state);
        assert_eq!(revealed.len(), 1 + 9 + 1);
        assert_eq!(revealed.last().map(String::as_str), Some("show:Fewer"));
    }

    #[test]
    fn children_render_only_when_expanded() {
        let records = sessions(2);
        let parent = SessionNode {
            session: &records[0],
            children: vec![node(&records[1])],
        };
        let project = ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/p".to_owned(),
        };
        let sections = vec![ProjectSection {
            project: &project,
            groups: vec![group("root", true, vec![parent])],
        }];
        let mut state = SidebarState::default();
        let status = HashMap::new();
        let build_kinds = |state: &SidebarState| {
            kinds(&build(
                &SidebarInputs {
                    chat_group: None,
                    pinned: &[],
                    work: &[],
                    sections: &sections,
                    state,
                    group_status: &status,
                    show_only_main_workspace: false,
                    active_project_id: None,
                },
                &mut SessionOrdering::new(),
            ))
        };
        assert_eq!(build_kinds(&state), ["project:app", "s00@0"]);
        state.toggle_expanded("project:active:s00");
        assert_eq!(build_kinds(&state), ["project:app", "s00@0", "s01@1"]);
    }

    #[test]
    fn pinned_sessions_move_to_their_zone() {
        let mut records = sessions(2);
        records[1].overlay.pinned_at = Some(1);
        let project = ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/p".to_owned(),
        };
        let sections = vec![ProjectSection {
            project: &project,
            groups: vec![group("root", true, records.iter().map(node).collect())],
        }];
        let mut ordering = SessionOrdering::new();
        let (pinned, work) = zone_items(&sections, None, &mut ordering);
        let state = SidebarState::default();
        let status = HashMap::new();
        let model = build(
            &SidebarInputs {
                chat_group: None,
                pinned: &pinned,
                work: &work,
                sections: &sections,
                state: &state,
                group_status: &status,
                show_only_main_workspace: false,
                active_project_id: None,
            },
            &mut ordering,
        );
        assert_eq!(
            kinds(&model),
            ["zone:pinned", "s01@0", "project:app", "s00@0"]
        );
        assert_eq!(model.sticky_header_for(3), Some(2));
    }

    #[test]
    fn a_failed_empty_group_shows_its_status() {
        let project = ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/p".to_owned(),
        };
        let sections = vec![ProjectSection {
            project: &project,
            groups: vec![group("root", true, Vec::new())],
        }];
        let state = SidebarState::default();
        let status = HashMap::from([("app:root".to_owned(), GroupLoadState::LoadFailed)]);
        let model = build(
            &SidebarInputs {
                chat_group: None,
                pinned: &[],
                work: &[],
                sections: &sections,
                state: &state,
                group_status: &status,
                show_only_main_workspace: false,
                active_project_id: None,
            },
            &mut SessionOrdering::new(),
        );
        assert_eq!(kinds(&model), ["project:app", "status:LoadFailed"]);
    }
}
