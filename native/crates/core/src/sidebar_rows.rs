//! The ordered sidebar row projection.
//!
//! Mirrors the normal mode of `buildSessionSidebarRowModel` in
//! `packages/ui/src/components/session/sidebar/sessionSidebarRowModel.ts` in
//! the Works view: Chats, Pinned and In work above projects, each listing
//! session works, linked worktrees and isolated spaces in lifecycle order. Group works expand
//! to their sessions. The earlier tree and timeline projections remain
//! explicit modes for their callers. Stable keys retain row state.

use std::collections::{HashMap, HashSet};

use crate::grouping::{GroupLabel, ProjectSection, SessionGroup, SessionNode};
use crate::ids::{ProjectId, SessionId};
use crate::ordering::SessionOrdering;
use crate::session::SessionRecord;
use crate::sidebar_work::{SidebarWork, SidebarWorkIndex, is_group_work};

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
    Timeline,
}

impl ActivityKey {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chats => "chats",
            Self::Pinned => "pinned",
            Self::Work => "work",
            Self::Timeline => "timeline",
        }
    }
}

/// How the projects are arranged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SidebarViewMode {
    /// Session and group works listed under their project.
    #[default]
    Works,
    /// Works from all projects in one lifecycle-ordered list.
    WorksByTime,
    /// One header per project over a flat, lifecycle-ordered list of its root
    /// sessions.
    Timeline,
    /// The project and worktree tree.
    Projects,
}

impl SidebarViewMode {
    fn uses_works(self) -> bool {
        matches!(self, Self::Works | Self::WorksByTime)
    }
}

/// How a session row is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RenderContext {
    /// A one-line tree row under a project.
    Project,
    /// A row under its project header in the timeline; two lines when zone
    /// rows are detailed.
    Timeline,
    /// The timeline row in Pinned and In work, where sessions of every project
    /// mix.
    TimelineMixed,
    /// A one-line Pinned or In work row of the tree view.
    MixedCompact,
    /// A one-line Chats row in the timeline.
    TimelineChat,
    /// A one-line Chats row in the tree view.
    TreeChat,
}

impl RenderContext {
    fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Timeline => "timeline",
            Self::TimelineMixed => "timeline-mixed",
            Self::MixedCompact => "mixed-compact",
            Self::TimelineChat => "timeline-chat",
            Self::TreeChat => "tree-chat",
        }
    }

    /// Legacy flat timeline rows never expand.
    fn expands(self) -> bool {
        !matches!(self, Self::Timeline | Self::TimelineMixed)
    }

    /// Rows that keep the tree's geometry and its slot left of the title.
    pub fn has_tree_slot(self) -> bool {
        matches!(self, Self::Project | Self::MixedCompact | Self::TreeChat)
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
    /// A linked worktree or isolated space, distinguished by its group label.
    Worktree {
        key: String,
        group_key: String,
        project_id: ProjectId,
        label: GroupLabel,
        expanded: bool,
        session_ids: Vec<SessionId>,
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
        /// Lines up with gutterless timeline rows instead of the tree level.
        flat: bool,
    },
    Status {
        key: String,
        group_key: String,
        state: GroupLoadState,
        depth: usize,
        /// Lines up with gutterless timeline rows instead of the tree level.
        flat: bool,
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
            | Self::Worktree { key, .. }
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
    /// Worktrees start folded; their expansion is shared across placements.
    pub expanded_works: HashSet<String>,
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
    pub view_mode: SidebarViewMode,
    /// Timeline and zone rows show their second line.
    pub detailed_zone_rows: bool,
    /// Whether In work also keeps its entries under their projects.
    pub keep_work_in_place: bool,
    pub chat_group: Option<&'v SessionGroup<'a>>,
    pub pinned: &'v [ZoneItem<'a>],
    pub work: &'v [ZoneItem<'a>],
    /// Every active root project session with its project, in lifecycle
    /// order, for the timeline.
    pub timeline: &'v [ZoneItem<'a>],
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

/// Builds the sidebar rows of the normal mode.
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
        work_index: if inputs.view_mode.uses_works() {
            SidebarWorkIndex::new(inputs.sections)
        } else {
            SidebarWorkIndex::default()
        },
    };
    builder.append_chats();
    builder.append_zone(ActivityKey::Pinned, inputs.pinned);
    if inputs.view_mode.uses_works() {
        builder.append_work_zone();
    } else {
        builder.append_zone(ActivityKey::Work, inputs.work);
    }
    match inputs.view_mode {
        SidebarViewMode::Works => builder.append_works_projects(),
        SidebarViewMode::WorksByTime => builder.append_works_by_time(),
        SidebarViewMode::Timeline => builder.append_timeline_projects(),
        SidebarViewMode::Projects => builder.append_projects(),
    }
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
    work_index: SidebarWorkIndex,
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
                context: match self.inputs.view_mode {
                    SidebarViewMode::Timeline => RenderContext::TimelineChat,
                    SidebarViewMode::Works
                    | SidebarViewMode::WorksByTime
                    | SidebarViewMode::Projects => RenderContext::TreeChat,
                },
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
        // Two-line rows with the project's icon in the timeline, one-line rows
        // in the tree unless zone rows are detailed.
        let context = if self.inputs.view_mode == SidebarViewMode::Timeline
            || self.inputs.detailed_zone_rows
        {
            RenderContext::TimelineMixed
        } else {
            RenderContext::MixedCompact
        };
        for item in items {
            self.append_trees(
                std::slice::from_ref(&item.node),
                &AppendOptions {
                    container_key: &container_key,
                    project_id: item.project_id.as_ref(),
                    archived: false,
                    context,
                    base_depth: 0,
                },
            );
        }
    }

    fn append_work_zone(&mut self) {
        let sessions: Vec<&SessionRecord> = self
            .inputs
            .work
            .iter()
            .map(|item| item.node.session)
            .collect();
        let works: Vec<SidebarWork> = self
            .work_index
            .ordered(&sessions, |_| true)
            .into_iter()
            .filter(|work| match work {
                SidebarWork::Session {
                    root_session_id, ..
                } => !self.pinned_in_zone.contains(root_session_id),
                SidebarWork::Group { .. } => true,
            })
            .cloned()
            .collect();
        if works.is_empty() || self.append_activity_header(ActivityKey::Work) {
            return;
        }
        for work in works {
            self.append_work(&work, "activity:work:");
        }
    }

    fn append_works_by_time(&mut self) {
        let mut sessions: Vec<&SessionRecord> = self
            .inputs
            .sections
            .iter()
            .flat_map(|section| &section.groups)
            .filter(|group| !group.is_archived_bucket)
            .flat_map(|group| group.sessions.iter().flat_map(SessionNode::preorder))
            .collect();
        sessions.sort_by(|left, right| {
            self.ordering
                .compare(left, right, is_pinned(left), is_pinned(right))
        });
        let working: HashSet<String> = self
            .inputs
            .work
            .iter()
            .filter_map(|item| self.work_index.work_for(&item.node.session.id))
            .map(SidebarWork::key)
            .collect();
        let mut works: Vec<SidebarWork> = self.work_index.ordered(&sessions, |_| true).into_iter()
            .filter(|work| !working.contains(&work.key()))
            .filter(|work| !matches!(work, SidebarWork::Session { root_session_id, .. } if self.pinned_in_zone.contains(root_session_id)))
            .cloned().collect();
        for work in self.work_index.groups() {
            if !working.contains(&work.key()) && !works.contains(work) {
                works.push(work.clone());
            }
        }
        let statuses: Vec<(String, GroupLoadState)> = self
            .inputs
            .sections
            .iter()
            .filter(|section| {
                !works.iter().any(|work| match work {
                    SidebarWork::Session { project_id, .. }
                    | SidebarWork::Group { project_id, .. } => project_id == &section.project.id,
                })
            })
            .flat_map(|section| {
                section.groups.iter().filter_map(|group| {
                    let key = format!("{}:{}", section.project.id, group.id);
                    self.inputs
                        .group_status
                        .get(&key)
                        .copied()
                        .filter(|state| *state != GroupLoadState::Ready)
                        .map(|state| (key, state))
                })
            })
            .collect();
        if (works.is_empty() && statuses.is_empty())
            || self.append_activity_header(ActivityKey::Timeline)
        {
            return;
        }
        for work in &works {
            self.append_work(work, "activity:timeline:");
        }
        for (key, state) in statuses {
            self.push_status(&key, state, 0, false);
        }
    }

    /// Works share a project's reveal limit, regardless of their kind.
    fn append_works_projects(&mut self) {
        let inputs = self.inputs;
        let working: HashSet<String> = inputs
            .work
            .iter()
            .filter_map(|item| self.work_index.work_for(&item.node.session.id))
            .filter(|_| !inputs.keep_work_in_place)
            .map(SidebarWork::key)
            .collect();
        for section in inputs.sections {
            let project_id = &section.project.id;
            let collapsed = inputs.state.collapsed_projects.contains(project_id);
            self.model.sticky_headers.push(self.model.rows.len());
            self.push(SidebarRow::ProjectHeader {
                key: format!("project:{project_id}:header"),
                project_id: project_id.clone(),
                collapsed,
            });
            if collapsed {
                continue;
            }
            let mut sessions: Vec<&SessionRecord> = section
                .groups
                .iter()
                .filter(|group| !group.is_archived_bucket)
                .flat_map(|group| group.sessions.iter().flat_map(SessionNode::preorder))
                .collect();
            sessions.sort_by(|left, right| {
                self.ordering
                    .compare(left, right, is_pinned(left), is_pinned(right))
            });
            let mut works: Vec<SidebarWork> = self.work_index.ordered(&sessions, |_| true).into_iter()
                .filter(|work| !working.contains(&work.key()))
                .filter(|work| !matches!(work, SidebarWork::Session { root_session_id, .. } if self.pinned_in_zone.contains(root_session_id)))
                .cloned().collect();
            for group in &section.groups {
                if is_group_work(group) {
                    let work = SidebarWork::Group {
                        project_id: project_id.clone(),
                        group_id: group.id.clone(),
                    };
                    if !working.contains(&work.key()) && !works.contains(&work) {
                        works.push(work);
                    }
                }
            }
            let container = format!("{project_id}:works");
            let count = inputs
                .state
                .visible_count_by_container
                .get(&container)
                .copied()
                .unwrap_or(SESSIONS_INITIAL_LIMIT)
                .max(SESSIONS_INITIAL_LIMIT)
                .min(works.len());
            for work in works.iter().take(count) {
                self.append_work(work, "");
            }
            if works.len() > SESSIONS_INITIAL_LIMIT {
                let (control, suffix) = if count < works.len() {
                    (ShowControl::More, "more")
                } else {
                    (ShowControl::Fewer, "fewer")
                };
                self.push(SidebarRow::ShowControl {
                    key: format!("{container}:{suffix}"),
                    control,
                    container_key: container,
                    current_count: count,
                    increment: SESSIONS_INCREMENT,
                    depth: 0,
                    flat: false,
                });
            }
            if works.is_empty() {
                for group in &section.groups {
                    let group_key = format!("{project_id}:{}", group.id);
                    if let Some(state) = inputs
                        .group_status
                        .get(&group_key)
                        .copied()
                        .filter(|state| *state != GroupLoadState::Ready)
                    {
                        self.push_status(&group_key, state, 0, false);
                    }
                }
            }
        }
    }

    fn append_work(&mut self, work: &SidebarWork, prefix: &str) {
        let inputs = self.inputs;
        match work {
            SidebarWork::Session {
                project_id,
                root_session_id,
            } => {
                let Some(section) = inputs
                    .sections
                    .iter()
                    .find(|section| &section.project.id == project_id)
                else {
                    return;
                };
                for group in &section.groups {
                    if let Some(node) = group
                        .sessions
                        .iter()
                        .find(|node| &node.session.id == root_session_id)
                    {
                        let container = if prefix.is_empty() {
                            format!("{project_id}:{}", group.id)
                        } else {
                            prefix.trim_end_matches(':').to_owned()
                        };
                        self.append_trees(
                            std::slice::from_ref(node),
                            &AppendOptions {
                                container_key: &container,
                                project_id: Some(project_id),
                                archived: false,
                                context: if prefix.is_empty() {
                                    RenderContext::Project
                                } else {
                                    RenderContext::MixedCompact
                                },
                                base_depth: 0,
                            },
                        );
                        return;
                    }
                }
            }
            SidebarWork::Group {
                project_id,
                group_id,
            } => {
                let Some(group) = inputs
                    .sections
                    .iter()
                    .find(|section| &section.project.id == project_id)
                    .and_then(|section| section.groups.iter().find(|group| &group.id == group_id))
                else {
                    return;
                };
                let group_key = format!("{project_id}:{group_id}");
                let expanded = inputs.state.expanded_works.contains(&group_key);
                let container = format!("{prefix}{group_key}");
                self.push(SidebarRow::Worktree {
                    key: format!("{container}:header"),
                    group_key: group_key.clone(),
                    project_id: project_id.clone(),
                    label: group.label.clone(),
                    expanded,
                    session_ids: group
                        .sessions
                        .iter()
                        .flat_map(SessionNode::preorder)
                        .map(|session| session.id.clone())
                        .collect(),
                });
                if expanded {
                    let nodes: Vec<&SessionNode<'_>> = group
                        .sessions
                        .iter()
                        .filter(|node| !self.pinned_in_zone.contains(&node.session.id))
                        .collect();
                    let count = inputs
                        .state
                        .visible_count_by_container
                        .get(&group_key)
                        .copied()
                        .unwrap_or(SESSIONS_INITIAL_LIMIT)
                        .max(SESSIONS_INITIAL_LIMIT)
                        .min(nodes.len());
                    for node in nodes.iter().take(count) {
                        self.append_trees(
                            std::slice::from_ref(node),
                            &AppendOptions {
                                container_key: &container,
                                project_id: Some(project_id),
                                archived: false,
                                context: RenderContext::Project,
                                base_depth: 1,
                            },
                        );
                    }
                    if nodes.len() > SESSIONS_INITIAL_LIMIT {
                        let (control, suffix) = if count < nodes.len() {
                            (ShowControl::More, "more")
                        } else {
                            (ShowControl::Fewer, "fewer")
                        };
                        self.push(SidebarRow::ShowControl {
                            key: format!("{container}:{suffix}"),
                            control,
                            container_key: group_key,
                            current_count: count,
                            increment: SESSIONS_INCREMENT,
                            depth: 1,
                            flat: false,
                        });
                    }
                }
            }
        }
    }

    /// One header per project, in the projects' order, over its root sessions
    /// from every worktree in lifecycle order. A session the Pinned zone lists
    /// is left out of its project; In work keeps it in both.
    fn append_timeline_projects(&mut self) {
        let inputs = self.inputs;
        let mut items_by_project: HashMap<&ProjectId, Vec<&'v ZoneItem<'a>>> = HashMap::new();
        for item in inputs.timeline {
            let Some(project_id) = item.project_id.as_ref() else {
                continue;
            };
            if self.pinned_in_zone.contains(&item.node.session.id) {
                continue;
            }
            items_by_project.entry(project_id).or_default().push(item);
        }
        for section in inputs.sections {
            let project_id = &section.project.id;
            let collapsed = inputs.state.collapsed_projects.contains(project_id);
            self.model.sticky_headers.push(self.model.rows.len());
            self.push(SidebarRow::ProjectHeader {
                key: format!("project:{project_id}:header"),
                project_id: project_id.clone(),
                collapsed,
            });
            if collapsed {
                continue;
            }
            // A space without chats has no row of its own here, so its group
            // header stands in for it, with the space's state, until its first
            // chat.
            for group in &section.groups {
                if group.space.is_none() || !group.sessions.is_empty() {
                    continue;
                }
                let group_key = format!("{project_id}:{}", group.id);
                let status = inputs.group_status.get(&group_key).copied();
                if status == Some(GroupLoadState::Loading) {
                    continue;
                }
                self.push(SidebarRow::GroupHeader {
                    key: format!("{group_key}:header"),
                    group_key: group_key.clone(),
                    project_id: Some(project_id.clone()),
                    label: group.label.clone(),
                    directory: group.directory.clone(),
                    collapsed: false,
                    empty: status.is_none_or(|state| state == GroupLoadState::Ready),
                });
                if let Some(state) = status.filter(|state| *state != GroupLoadState::Ready) {
                    self.push_status(&group_key, state, 0, true);
                }
            }
            let items = items_by_project.remove(project_id).unwrap_or_default();
            if !items.is_empty() {
                self.append_revealed(
                    &items,
                    &format!("timeline-project:{project_id}"),
                    RenderContext::Timeline,
                    SESSIONS_INITIAL_LIMIT,
                    SESSIONS_INCREMENT,
                );
                continue;
            }
            let Some(main) = section.groups.iter().find(|group| group.is_main) else {
                continue;
            };
            let group_key = format!("{project_id}:{}", main.id);
            if let Some(state) = inputs
                .group_status
                .get(&group_key)
                .copied()
                .filter(|state| *state != GroupLoadState::Ready)
            {
                self.push_status(&group_key, state, 0, true);
            }
        }
    }

    /// A list that reveals `initial` entries, then `increment` more per Show
    /// more.
    fn append_revealed(
        &mut self,
        items: &[&'v ZoneItem<'a>],
        container_key: &str,
        context: RenderContext,
        initial: usize,
        increment: usize,
    ) {
        let requested = self
            .inputs
            .state
            .visible_count_by_container
            .get(container_key)
            .copied()
            .unwrap_or(initial)
            .max(initial);
        let visible = &items[..requested.min(items.len())];
        for item in visible {
            self.append_trees(
                std::slice::from_ref(&item.node),
                &AppendOptions {
                    container_key,
                    project_id: item.project_id.as_ref(),
                    archived: false,
                    context,
                    base_depth: 0,
                },
            );
        }
        let control = if items.len() > visible.len() {
            Some(ShowControl::More)
        } else if items.len() > initial {
            Some(ShowControl::Fewer)
        } else {
            None
        };
        if let Some(control) = control {
            let suffix = match control {
                ShowControl::More => "more",
                ShowControl::Fewer => "fewer",
            };
            self.push(SidebarRow::ShowControl {
                key: format!("{container_key}:{suffix}"),
                control,
                container_key: container_key.to_owned(),
                current_count: visible.len(),
                increment,
                depth: 0,
                flat: true,
            });
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
                flat: false,
            });
        } else if !group.is_archived_bucket && limited_count > initial && remaining == 0 {
            self.push(SidebarRow::ShowControl {
                key: format!("{group_key}:fewer"),
                control: ShowControl::Fewer,
                container_key: group_key.to_owned(),
                current_count: limited_visible,
                increment,
                depth: content_depth,
                flat: false,
            });
        }

        match status {
            Some(state) if source.is_empty() && state != GroupLoadState::Ready => {
                self.push_status(group_key, state, content_depth, false);
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
                self.push_status(group_key, state, content_depth, false);
            }
            _ => {}
        }
    }

    fn push_status(&mut self, group_key: &str, state: GroupLoadState, depth: usize, flat: bool) {
        self.push(SidebarRow::Status {
            key: format!("{group_key}:status"),
            group_key: group_key.to_owned(),
            state,
            depth,
            flat,
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

/// The gap below a row that opens the next section, as `sectionSpacingAfter`
/// in `packages/ui/src/components/session/sidebar/sessionSidebarVirtualization.ts`
/// sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SectionSpacing {
    /// Before a zone or project header (`pb-2`): sections of equal weight.
    Section,
    /// Before a worktree header inside its project (`pb-1.5`).
    Group,
}

/// The gap below `row` when `next` follows it.
pub fn section_spacing_after(
    row: &SidebarRow<'_>,
    next: Option<&SidebarRow<'_>>,
) -> Option<SectionSpacing> {
    match next? {
        SidebarRow::ActivityHeader { .. } | SidebarRow::ProjectHeader { .. } => {
            Some(SectionSpacing::Section)
        }
        SidebarRow::GroupHeader { .. } if !matches!(row, SidebarRow::ProjectHeader { .. }) => {
            Some(SectionSpacing::Group)
        }
        _ => None,
    }
}

/// Every active root project session as a flat timeline item, in lifecycle
/// order (pinned first), with the project that owns it.
pub fn timeline_items<'a>(
    sections: &[ProjectSection<'a>],
    ordering: &mut SessionOrdering,
) -> Vec<ZoneItem<'a>> {
    let mut items: Vec<ZoneItem<'a>> = sections
        .iter()
        .flat_map(|section| {
            section
                .groups
                .iter()
                .filter(|group| !group.is_archived_bucket)
                .flat_map(move |group| {
                    group.sessions.iter().map(move |node| ZoneItem {
                        // Timeline rows never expand.
                        node: SessionNode {
                            session: node.session,
                            children: Vec::new(),
                        },
                        project_id: Some(section.project.id.clone()),
                    })
                })
        })
        .collect();
    items.sort_by(|left, right| {
        ordering.compare(
            left.node.session,
            right.node.session,
            is_pinned(left.node.session),
            is_pinned(right.node.session),
        )
    });
    items
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
                SidebarRow::Worktree { group_key, .. } => format!("worktree:{group_key}"),
                SidebarRow::Session { session, depth, .. } => format!("{}@{depth}", session.id),
                SidebarRow::ShowControl { control, .. } => format!("show:{control:?}"),
                SidebarRow::Status { state, .. } => format!("status:{state:?}"),
                SidebarRow::Empty { kind, .. } => format!("empty:{kind:?}"),
            })
            .collect()
    }

    fn timeline_inputs<'v, 'a>(
        sections: &'v [ProjectSection<'a>],
        timeline: &'v [ZoneItem<'a>],
        work: &'v [ZoneItem<'a>],
        state: &'v SidebarState,
        group_status: &'v HashMap<String, GroupLoadState>,
    ) -> SidebarInputs<'v, 'a> {
        SidebarInputs {
            view_mode: SidebarViewMode::Timeline,
            detailed_zone_rows: false,
            keep_work_in_place: false,
            chat_group: None,
            pinned: &[],
            work,
            timeline,
            sections,
            state,
            group_status,
            show_only_main_workspace: false,
            active_project_id: None,
        }
    }

    #[test]
    fn works_group_worktrees_and_share_expansion_across_placements() {
        let records = sessions(3);
        let project = ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/p".to_owned(),
        };
        let mut feature = group("feature", false, vec![node(&records[1]), node(&records[2])]);
        feature.label = GroupLabel::Worktree {
            label: "feature".to_owned(),
        };
        let mut empty = group("empty", false, Vec::new());
        empty.label = GroupLabel::Worktree {
            label: "empty".to_owned(),
        };
        let sections = [ProjectSection {
            project: &project,
            groups: vec![group("root", true, vec![node(&records[0])]), feature, empty],
        }];
        let statuses = HashMap::new();
        let state = SidebarState::default();
        let mut ordering = SessionOrdering::new();
        let mut input = timeline_inputs(&sections, &[], &[], &state, &statuses);
        input.view_mode = SidebarViewMode::Works;
        assert_eq!(
            kinds(&build(&input, &mut ordering)),
            [
                "project:app",
                "s00@0",
                "worktree:app:feature",
                "worktree:app:empty"
            ]
        );

        let mut expanded_state = state.clone();
        expanded_state
            .expanded_works
            .insert("app:feature".to_owned());
        input.state = &expanded_state;
        assert_eq!(
            kinds(&build(&input, &mut ordering)),
            [
                "project:app",
                "s00@0",
                "worktree:app:feature",
                "s01@1",
                "s02@1",
                "worktree:app:empty"
            ]
        );

        let work = [ZoneItem {
            node: node(&records[1]),
            project_id: Some(project.id.clone()),
        }];
        input.work = &work;
        let model = build(&input, &mut ordering);
        assert_eq!(
            kinds(&model),
            [
                "zone:work",
                "worktree:app:feature",
                "s01@1",
                "s02@1",
                "project:app",
                "s00@0",
                "worktree:app:empty"
            ]
        );
        assert_eq!(model.rows.iter().filter(|row| matches!(row, SidebarRow::Worktree { group_key, .. } if group_key == "app:feature")).count(), 1);
    }

    #[test]
    fn works_group_space_sessions_and_retain_empty_spaces() {
        use crate::ids::SpaceId;

        let records = sessions(3);
        let project = ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/p".to_owned(),
        };
        let mut root = node(&records[0]);
        root.children.push(node(&records[1]));
        let mut space = group("space", false, vec![root, node(&records[2])]);
        space.space = Some(SpaceId::from("space"));
        space.label = GroupLabel::Space {
            name: Some("space".to_owned()),
        };
        let mut empty = group("empty-space", false, Vec::new());
        empty.space = Some(SpaceId::from("empty-space"));
        empty.label = GroupLabel::Space {
            name: Some("empty-space".to_owned()),
        };
        let sections = [ProjectSection {
            project: &project,
            groups: vec![space, empty],
        }];
        let statuses = HashMap::new();
        let mut state = SidebarState::default();
        state.expanded_works.insert("app:space".to_owned());
        let work = [ZoneItem {
            node: node(&records[2]),
            project_id: Some(project.id.clone()),
        }];
        for mode in [SidebarViewMode::Works, SidebarViewMode::WorksByTime] {
            for in_work in [false, true] {
                let mut ordering = SessionOrdering::new();
                let mut input = timeline_inputs(&sections, &[], &[], &state, &statuses);
                input.view_mode = mode;
                if in_work {
                    input.work = &work;
                }
                let model = build(&input, &mut ordering);
                let groups: Vec<_> = model
                    .rows
                    .iter()
                    .filter_map(|row| match row {
                        SidebarRow::Worktree {
                            group_key,
                            label,
                            session_ids,
                            ..
                        } => {
                            assert!(
                                matches!(label, GroupLabel::Space { .. }),
                                "space label survives projection"
                            );
                            Some((group_key.as_str(), session_ids.len()))
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    groups,
                    [("app:space", 3), ("app:empty-space", 0)],
                    "spaces appear once in {mode:?}, in work {in_work}"
                );
                assert!(
                    !model
                        .rows
                        .iter()
                        .any(|row| matches!(row, SidebarRow::GroupHeader { .. })),
                    "empty spaces use the same work row as populated spaces"
                );
                assert!(model.rows.iter().any(|row| matches!(row, SidebarRow::Session { session, depth: 1, .. } if session.id == records[2].id)), "expanded spaces show their root sessions");
            }
        }
    }

    #[test]
    fn works_do_not_reintroduce_a_pinned_session_when_its_zone_is_collapsed() {
        let records = sessions(2);
        let project = ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/p".to_owned(),
        };
        let sections = [ProjectSection {
            project: &project,
            groups: vec![group("root", true, records.iter().map(node).collect())],
        }];
        let statuses = HashMap::new();
        let mut state = SidebarState::default();
        state.collapsed_activities.insert(ActivityKey::Pinned);
        let pinned = [ZoneItem {
            node: node(&records[0]),
            project_id: Some(project.id.clone()),
        }];
        let mut input = timeline_inputs(&sections, &[], &[], &state, &statuses);
        input.view_mode = SidebarViewMode::Works;
        input.pinned = &pinned;
        assert_eq!(
            kinds(&build(&input, &mut SessionOrdering::new())),
            ["zone:pinned", "project:app", "s01@0"]
        );
    }

    #[test]
    fn expanded_worktrees_reveal_seven_sessions_before_more() {
        let records = sessions(10);
        let project = ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/p".to_owned(),
        };
        let mut feature = group("feature", false, records.iter().map(node).collect());
        feature.label = GroupLabel::Worktree {
            label: "feature".to_owned(),
        };
        let sections = [ProjectSection {
            project: &project,
            groups: vec![feature],
        }];
        let statuses = HashMap::new();
        let mut state = SidebarState::default();
        state.expanded_works.insert("app:feature".to_owned());
        let mut input = timeline_inputs(&sections, &[], &[], &state, &statuses);
        input.view_mode = SidebarViewMode::Works;
        let mut ordering = SessionOrdering::new();
        let initial = build(&input, &mut ordering);
        assert_eq!(initial.session_order.len(), 7);
        assert!(
            matches!(initial.rows.last(), Some(SidebarRow::ShowControl { control: ShowControl::More, container_key, current_count: 7, .. }) if container_key == "app:feature")
        );
        let mut more = state.clone();
        more.reveal("app:feature", ShowControl::More, 7, 7);
        input.state = &more;
        let expanded = build(&input, &mut ordering);
        assert_eq!(expanded.session_order.len(), 10);
        assert!(matches!(
            expanded.rows.last(),
            Some(SidebarRow::ShowControl {
                control: ShowControl::Fewer,
                ..
            })
        ));
    }

    #[test]
    fn works_by_time_has_no_project_reveal_limit_or_duplicate_work() {
        let records = sessions(12);
        let project = ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/p".to_owned(),
        };
        let sections = [ProjectSection {
            project: &project,
            groups: vec![group("root", true, records.iter().map(node).collect())],
        }];
        let statuses = HashMap::new();
        let state = SidebarState::default();
        let work = [ZoneItem {
            node: node(&records[0]),
            project_id: Some(project.id.clone()),
        }];
        let mut input = timeline_inputs(&sections, &[], &work, &state, &statuses);
        input.view_mode = SidebarViewMode::WorksByTime;
        let model = build(&input, &mut SessionOrdering::new());
        assert_eq!(model.session_order.len(), 12);
        assert_eq!(
            model
                .session_order
                .iter()
                .filter(|id| *id == &records[0].id)
                .count(),
            1
        );
        assert!(!model.rows.iter().any(|row| matches!(
            row,
            SidebarRow::ShowControl { .. } | SidebarRow::ProjectHeader { .. }
        )));
        assert!(model.rows.iter().any(|row| matches!(
            row,
            SidebarRow::ActivityHeader {
                activity: ActivityKey::Timeline,
                ..
            }
        )));

        input.view_mode = SidebarViewMode::Works;
        input.keep_work_in_place = true;
        let kept = build(&input, &mut SessionOrdering::new());
        assert_eq!(
            kept.session_order
                .iter()
                .filter(|id| *id == &records[0].id)
                .count(),
            2,
            "keeping a work in place shows both placements"
        );
    }

    #[test]
    fn sections_open_with_a_gap_and_worktrees_with_a_smaller_one() {
        let project_header = SidebarRow::ProjectHeader {
            key: "project:p:header".to_owned(),
            project_id: ProjectId::from("p"),
            collapsed: false,
        };
        let zone_header = SidebarRow::ActivityHeader {
            key: "activity:work:header".to_owned(),
            activity: ActivityKey::Work,
            collapsed: false,
        };
        let group_header = SidebarRow::GroupHeader {
            key: "p:g:header".to_owned(),
            group_key: "p:g".to_owned(),
            project_id: Some(ProjectId::from("p")),
            label: GroupLabel::ProjectRoot { branch: None },
            directory: None,
            collapsed: false,
            empty: false,
        };
        let status = SidebarRow::Status {
            key: "p:g:status".to_owned(),
            group_key: "p:g".to_owned(),
            state: GroupLoadState::Loading,
            depth: 0,
            flat: false,
        };
        assert_eq!(
            section_spacing_after(&zone_header, Some(&project_header)),
            Some(SectionSpacing::Section)
        );
        assert_eq!(
            section_spacing_after(&status, Some(&zone_header)),
            Some(SectionSpacing::Section)
        );
        assert_eq!(
            section_spacing_after(&status, Some(&group_header)),
            Some(SectionSpacing::Group)
        );
        assert_eq!(
            section_spacing_after(&project_header, Some(&group_header)),
            None
        );
        assert_eq!(section_spacing_after(&group_header, Some(&status)), None);
        assert_eq!(section_spacing_after(&status, None), None);
    }

    #[test]
    fn the_timeline_lists_root_sessions_under_their_project_seven_at_a_time() {
        let records = sessions(9);
        let nodes: Vec<SessionNode<'_>> = records.iter().map(node).collect();
        let project = ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/p".to_owned(),
        };
        let sections = vec![ProjectSection {
            project: &project,
            groups: vec![group("root", true, nodes)],
        }];
        let mut ordering = SessionOrdering::new();
        let timeline = timeline_items(&sections, &mut ordering);
        let work = vec![timeline[0].clone()];
        let state = SidebarState::default();
        let status = HashMap::new();
        let model = build(
            &timeline_inputs(&sections, &timeline, &work, &state, &status),
            &mut ordering,
        );
        let keys: Vec<&str> = model.rows.iter().map(SidebarRow::key).collect();
        assert_eq!(keys[0], "activity:work:header");
        assert_eq!(keys[1], "activity:work:session:s00");
        assert_eq!(keys[2], "project:app:header");
        assert_eq!(
            keys[3], "timeline-project:app:session:s00",
            "In work keeps it in its project"
        );
        assert_eq!(keys.len(), 3 + 7 + 1, "seven sessions and a Show more row");
        assert_eq!(keys[10], "timeline-project:app:more");
        assert!(matches!(
            model.rows[1],
            SidebarRow::Session {
                context: RenderContext::TimelineMixed,
                has_children: false,
                ..
            }
        ));
        assert!(matches!(
            model.rows[3],
            SidebarRow::Session {
                context: RenderContext::Timeline,
                ..
            }
        ));
        assert!(matches!(
            model.rows[10],
            SidebarRow::ShowControl {
                flat: true,
                current_count: 7,
                ..
            }
        ));

        let mut revealed = SidebarState::default();
        revealed.reveal("timeline-project:app", ShowControl::More, 7, 7);
        let model = build(
            &timeline_inputs(&sections, &timeline, &work, &revealed, &status),
            &mut ordering,
        );
        assert_eq!(
            model.rows.last().map(SidebarRow::key),
            Some("timeline-project:app:fewer")
        );
    }

    #[test]
    fn an_empty_timeline_project_shows_its_main_group_status() {
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
        let mut ordering = SessionOrdering::new();
        let model = build(
            &timeline_inputs(&sections, &[], &[], &state, &status),
            &mut ordering,
        );
        let keys: Vec<&str> = model.rows.iter().map(SidebarRow::key).collect();
        assert_eq!(keys, ["project:app:header", "app:root:status"]);
    }

    #[test]
    fn an_empty_sidebar_shows_the_empty_row() {
        let state = SidebarState::default();
        let status = HashMap::new();
        let model = build(
            &SidebarInputs {
                view_mode: SidebarViewMode::Projects,
                detailed_zone_rows: false,
                keep_work_in_place: false,
                timeline: &[],
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
                    view_mode: SidebarViewMode::Projects,
                    detailed_zone_rows: false,
                    keep_work_in_place: false,
                    timeline: &[],
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
                    view_mode: SidebarViewMode::Projects,
                    detailed_zone_rows: false,
                    keep_work_in_place: false,
                    timeline: &[],
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
                view_mode: SidebarViewMode::Projects,
                detailed_zone_rows: false,
                keep_work_in_place: false,
                timeline: &[],
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
                view_mode: SidebarViewMode::Projects,
                detailed_zone_rows: false,
                keep_work_in_place: false,
                timeline: &[],
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
