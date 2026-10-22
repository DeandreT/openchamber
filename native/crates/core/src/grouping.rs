//! Session trees and the project/worktree groups of the sidebar.
//!
//! Mirrors `buildGroupedSessions` in
//! `packages/ui/src/components/session/sidebar/projects/useSessionGrouping.ts`
//! and `buildSidebarChatGroup` in `sessionChatProjection.ts`. Labels stay as
//! data; the renderer resolves them through the locale resources.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::chat_roots::ChatRoots;
use crate::ids::{ProjectId, SessionId, SpaceId};
use crate::ordering::SessionOrdering;
use crate::ownership::{
    ConfiguredProject, SessionOwnershipIndex, SpaceMark, Surface, WorktreeInfo,
};
use crate::path;
use crate::session::SessionRecord;

/// A session and its visible subsessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionNode<'a> {
    pub session: &'a SessionRecord,
    pub children: Vec<SessionNode<'a>>,
}

impl<'a> SessionNode<'a> {
    /// The node's session followed by its descendants in preorder.
    pub fn preorder(&self) -> Vec<&'a SessionRecord> {
        let mut sessions = Vec::new();
        let mut stack: Vec<&SessionNode<'a>> = vec![self];
        while let Some(node) = stack.pop() {
            sessions.push(node.session);
            for child in node.children.iter().rev() {
                stack.push(child);
            }
        }
        sessions
    }
}

/// How a group header is labelled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupLabel {
    /// Managed Chats; the zone header carries the label.
    Chats,
    /// The project root, with its branch when the project is a repository.
    ProjectRoot { branch: Option<String> },
    /// A linked worktree, labelled from its metadata or directory name.
    Worktree { label: String },
    /// An isolated space; `None` when the space is unnamed.
    Space { name: Option<String> },
    /// The project's archived sessions.
    Archived,
}

/// What a group's new-session action creates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DraftTarget {
    Chat,
    Project,
}

/// One group of sessions under a project, or the Chats group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionGroup<'a> {
    pub id: String,
    pub label: GroupLabel,
    pub directory: Option<String>,
    pub is_main: bool,
    pub is_archived_bucket: bool,
    pub space: Option<SpaceId>,
    pub draft_target: DraftTarget,
    pub sessions: Vec<SessionNode<'a>>,
}

/// A configured project and its groups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSection<'a> {
    pub project: &'a ConfiguredProject,
    pub groups: Vec<SessionGroup<'a>>,
}

/// How worktree groups are ordered inside a project.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WorktreeSortOrder {
    /// Alphabetical, with the user's saved drag order applied by the caller.
    #[default]
    Manual,
    Alphabetical,
    /// Populated worktrees by latest session activity, then by label.
    Recent,
}

/// What project grouping reads besides the project's own sessions.
#[derive(Clone, Copy, Debug)]
pub struct GroupingContext<'a, 'o> {
    pub ownership: &'o SessionOwnershipIndex<'a>,
    pub spaces: &'a [SpaceMark],
    pub surface: Surface,
    pub worktree_sort: WorktreeSortOrder,
    /// Live Git branch by normalized directory.
    pub branches: &'o HashMap<String, String>,
}

const ARCHIVED_GROUP_KEY: &str = "__archived__";
const PROJECT_ROOT_KEY: &str = "__project_root__";

/// Builds a project's groups from its active and archived sessions.
///
/// `sessions` must already be in lifecycle order. Only active subsessions nest
/// under active parents; a subsession under an archived parent, or archived
/// itself, is hidden, while one whose parent is not loaded stays a root. A
/// malformed parent cycle surfaces from its first input row.
pub fn build_project_section<'a>(
    project: &'a ConfiguredProject,
    sessions: &[&'a SessionRecord],
    worktrees: &[WorktreeInfo],
    context: &GroupingContext<'a, '_>,
    ordering: &mut SessionOrdering,
) -> ProjectSection<'a> {
    let root = path::normalize(&project.path);
    let sessions = dedupe(sessions);
    let by_id: HashMap<&SessionId, &SessionRecord> = sessions
        .iter()
        .map(|session| (&session.id, *session))
        .collect();
    let mut children: HashMap<&SessionId, Vec<&'a SessionRecord>> = HashMap::new();
    for session in &sessions {
        let Some(parent_id) = &session.parent_id else {
            continue;
        };
        let Some(parent) = by_id.get(parent_id) else {
            continue;
        };
        if parent.is_archived() || session.is_archived() {
            continue;
        }
        children.entry(parent_id).or_default().push(session);
    }
    let is_hidden_subsession = |session: &SessionRecord| match &session.parent_id {
        None => false,
        Some(parent_id) => {
            session.is_archived()
                || by_id
                    .get(parent_id)
                    .is_some_and(|parent| parent.is_archived())
        }
    };

    let mut claimed: HashSet<&SessionId> = HashSet::new();
    let mut roots: Vec<SessionNode<'a>> = Vec::new();
    let root_candidates = sessions.iter().filter(|session| match &session.parent_id {
        None => true,
        Some(parent_id) => !by_id.contains_key(parent_id) && !is_hidden_subsession(session),
    });
    for session in root_candidates {
        if !claimed.contains(&session.id) {
            roots.push(build_tree(session, &children, &mut claimed));
        }
    }
    for session in &sessions {
        if !is_hidden_subsession(session) && !claimed.contains(&session.id) {
            roots.push(build_tree(session, &children, &mut claimed));
        }
    }

    let worktree_by_path = worktree_index(worktrees, root.as_deref());
    let space_by_directory: Vec<(String, &SpaceMark)> = if context.surface == Surface::Desktop {
        let mut seen = HashSet::new();
        context
            .spaces
            .iter()
            .filter(|space| path::normalize_optional(space.project_directory.as_deref()) == root)
            .filter_map(|space| {
                let directory = path::normalize_optional(space.directory.as_deref())?;
                seen.insert(directory.clone()).then_some((directory, space))
            })
            .collect()
    } else {
        Vec::new()
    };
    let root_key = root.clone().unwrap_or_else(|| PROJECT_ROOT_KEY.to_owned());
    let group_key = |session: &SessionRecord| -> String {
        if session.is_archived() {
            return ARCHIVED_GROUP_KEY.to_owned();
        }
        if context.surface == Surface::VsCode {
            return root_key.clone();
        }
        if let Some(owner) = context.ownership.owner(&session.id) {
            let scope = &owner.scope_directory;
            if Some(scope) == root.as_ref() {
                return root_key.clone();
            }
            if space_by_directory
                .iter()
                .any(|(directory, _)| directory == scope)
            {
                return scope.clone();
            }
            if let Some(worktree) = find_prefix_worktree(scope, &worktree_by_path) {
                return worktree.to_owned();
            }
        }
        let Some(directory) = session.resolved_directory() else {
            return root_key.clone();
        };
        if Some(&directory) == root.as_ref() {
            return root_key.clone();
        }
        find_prefix_worktree(&directory, &worktree_by_path)
            .map(str::to_owned)
            .unwrap_or_else(|| root_key.clone())
    };

    let mut grouped: HashMap<String, Vec<SessionNode<'a>>> = HashMap::new();
    for node in roots {
        grouped
            .entry(group_key(node.session))
            .or_default()
            .push(node);
    }

    let mut groups = vec![SessionGroup {
        id: "root".to_owned(),
        label: GroupLabel::ProjectRoot {
            branch: root
                .as_ref()
                .and_then(|root| context.branches.get(root))
                .filter(|branch| branch.as_str() != "HEAD")
                .cloned(),
        },
        directory: root.clone(),
        is_main: true,
        is_archived_bucket: false,
        space: None,
        draft_target: DraftTarget::Project,
        sessions: grouped.remove(&root_key).unwrap_or_default(),
    }];

    if context.surface == Surface::Desktop {
        let mut ordered: Vec<(&String, &WorktreeInfo)> = worktree_by_path
            .iter()
            .map(|(path, info)| (path, *info))
            .collect();
        sort_worktrees(&mut ordered, &grouped, context.worktree_sort, ordering);
        for (directory, info) in ordered {
            let branch = context
                .branches
                .get(directory)
                .map(|branch| branch.trim().to_owned())
                .filter(|branch| !branch.is_empty());
            groups.push(SessionGroup {
                id: format!("worktree:{directory}"),
                label: GroupLabel::Worktree {
                    label: worktree_label(info, branch.as_deref(), directory),
                },
                directory: Some(directory.clone()),
                is_main: false,
                is_archived_bucket: false,
                space: None,
                draft_target: DraftTarget::Project,
                sessions: grouped.remove(directory).unwrap_or_default(),
            });
        }
        for (directory, space) in &space_by_directory {
            groups.push(SessionGroup {
                id: format!("space:{}", space.id),
                label: GroupLabel::Space { name: None },
                directory: Some(directory.clone()),
                is_main: false,
                is_archived_bucket: false,
                space: Some(space.id.clone()),
                draft_target: DraftTarget::Project,
                sessions: grouped.remove(directory).unwrap_or_default(),
            });
        }
    }

    let archived = grouped.remove(ARCHIVED_GROUP_KEY).unwrap_or_default();
    if !archived.is_empty() {
        groups.push(SessionGroup {
            id: "archived".to_owned(),
            label: GroupLabel::Archived,
            directory: None,
            is_main: false,
            is_archived_bucket: true,
            space: None,
            draft_target: DraftTarget::Project,
            sessions: archived,
        });
    }

    ProjectSection { project, groups }
}

/// Builds the Chats group from Chats members.
///
/// Returns `None` when no Chats root is known and there are no members.
pub fn build_chat_group<'a>(
    chat_sessions: &[&'a SessionRecord],
    roots: Option<&ChatRoots>,
) -> Option<SessionGroup<'a>> {
    let chats_root = roots
        .map(|roots| roots.configured().to_owned())
        .or_else(|| {
            chat_sessions
                .iter()
                .find_map(|session| {
                    roots.and_then(|roots| roots.root_of(session.directory.as_deref()))
                })
                .map(str::to_owned)
        });
    if chats_root.is_none() && chat_sessions.is_empty() {
        return None;
    }
    let mut children: HashMap<&SessionId, Vec<&'a SessionRecord>> = HashMap::new();
    for session in chat_sessions {
        if let Some(parent_id) = &session.parent_id {
            children.entry(parent_id).or_default().push(session);
        }
    }
    let mut claimed = HashSet::new();
    let sessions = chat_sessions
        .iter()
        .filter(|session| session.is_root())
        .map(|session| build_tree(session, &children, &mut claimed))
        .collect();
    Some(SessionGroup {
        id: "managed-chats".to_owned(),
        label: GroupLabel::Chats,
        directory: chats_root,
        is_main: true,
        is_archived_bucket: false,
        space: None,
        draft_target: DraftTarget::Chat,
        sessions,
    })
}

/// The project ID owning each section, for lookups by callers.
pub fn section_index<'s, 'a>(sections: &'s [ProjectSection<'a>]) -> HashMap<&'s ProjectId, usize> {
    sections
        .iter()
        .enumerate()
        .map(|(index, section)| (&section.project.id, index))
        .collect()
}

fn build_tree<'a>(
    session: &'a SessionRecord,
    children: &HashMap<&SessionId, Vec<&'a SessionRecord>>,
    claimed: &mut HashSet<&'a SessionId>,
) -> SessionNode<'a> {
    claimed.insert(&session.id);
    let mut nodes = Vec::new();
    for child in children.get(&session.id).into_iter().flatten() {
        if child.is_archived() || claimed.contains(&child.id) {
            continue;
        }
        nodes.push(build_tree(child, children, claimed));
    }
    SessionNode {
        session,
        children: nodes,
    }
}

fn dedupe<'a>(sessions: &[&'a SessionRecord]) -> Vec<&'a SessionRecord> {
    let mut seen = HashSet::new();
    sessions
        .iter()
        .copied()
        .filter(|session| seen.insert(&session.id))
        .collect()
}

/// Normalized worktree paths excluding the project root, first entry wins.
fn worktree_index<'w>(
    worktrees: &'w [WorktreeInfo],
    root: Option<&str>,
) -> Vec<(String, &'w WorktreeInfo)> {
    let mut index: Vec<(String, &WorktreeInfo)> = Vec::new();
    for info in worktrees {
        let Some(directory) = path::normalize(&info.path) else {
            continue;
        };
        if Some(directory.as_str()) == root || index.iter().any(|(path, _)| *path == directory) {
            continue;
        }
        index.push((directory, info));
    }
    index
}

/// The longest indexed worktree containing `directory`.
fn find_prefix_worktree<'i>(
    directory: &str,
    index: &'i [(String, &WorktreeInfo)],
) -> Option<&'i str> {
    index
        .iter()
        .filter(|(path, _)| path::is_within(directory, path))
        .max_by_key(|(path, _)| path.len())
        .map(|(path, _)| path.as_str())
}

fn worktree_label(info: &WorktreeInfo, live_branch: Option<&str>, directory: &str) -> String {
    let fallback = directory.rsplit('/').next().unwrap_or(directory).to_owned();
    let label = info
        .label
        .clone()
        .filter(|label| !label.is_empty())
        .or_else(|| info.name.clone().filter(|name| !name.is_empty()))
        .unwrap_or(fallback);
    let Some(live_branch) = live_branch else {
        return label;
    };
    let listed = info
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty());
    let label_is_branch = match listed {
        Some(listed) => info
            .label
            .as_deref()
            .is_some_and(|label| branch_key(label) == branch_key(listed)),
        None => true,
    };
    if label_is_branch {
        live_branch.to_owned()
    } else {
        label
    }
}

/// Mirrors `normalizeForBranchComparison`: case-folded, without an `opencode`
/// prefix, `-` or `_`.
fn branch_key(branch: &str) -> String {
    let lower = branch.to_lowercase();
    let unprefixed = lower
        .strip_prefix("opencode")
        .map(|rest| rest.strip_prefix(['/', '-']).unwrap_or(rest))
        .unwrap_or(&lower);
    unprefixed
        .chars()
        .filter(|character| !matches!(character, '-' | '_'))
        .collect::<String>()
        .trim()
        .to_owned()
}

fn display_label(info: &WorktreeInfo) -> String {
    [&info.label, &info.branch, &info.name]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| info.path.clone())
        .to_lowercase()
}

fn sort_worktrees(
    worktrees: &mut [(&String, &WorktreeInfo)],
    grouped: &HashMap<String, Vec<SessionNode<'_>>>,
    order: WorktreeSortOrder,
    ordering: &mut SessionOrdering,
) {
    if order != WorktreeSortOrder::Recent {
        worktrees.sort_by_key(|(_, info)| display_label(info));
        return;
    }
    let mut activity: HashMap<&String, (bool, i64)> = HashMap::new();
    for (directory, _) in worktrees.iter() {
        let nodes = grouped
            .get(*directory)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let latest = nodes
            .iter()
            .map(|node| ordering.order_value(node.session, false))
            .max()
            .unwrap_or(0);
        activity.insert(directory, (!nodes.is_empty(), latest));
    }
    worktrees.sort_by(|(left_dir, left), (right_dir, right)| {
        let (left_active, left_latest) = activity[left_dir];
        let (right_active, right_latest) = activity[right_dir];
        right_active
            .cmp(&left_active)
            .then_with(|| {
                if left_active && right_active {
                    right_latest.cmp(&left_latest)
                } else {
                    Ordering::Equal
                }
            })
            .then_with(|| display_label(left).cmp(&display_label(right)))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ownership::{OwnershipInputs, ProjectWorktrees};

    fn child(id: &str, parent: &str, directory: &str) -> SessionRecord {
        let mut session = SessionRecord::new(id, directory);
        session.parent_id = Some(SessionId::from(parent));
        session
    }

    #[test]
    fn groups_root_worktree_and_archived_sessions() {
        let project = ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/p/app".to_owned(),
        };
        let mut archived = SessionRecord::new("old", "/p/app");
        archived.time.archived = Some(5);
        let sessions = vec![
            SessionRecord::new("main", "/p/app"),
            child("sub", "main", "/p/app"),
            SessionRecord::new("feature", "/w/feature/src"),
            archived,
            child("orphan", "not-loaded", "/p/app"),
        ];
        let worktrees = vec![WorktreeInfo::new("/w/feature")];
        let projects = [project.clone()];
        let topology = [ProjectWorktrees {
            project_path: "/p/app".to_owned(),
            worktrees: worktrees.clone(),
        }];
        let ownership = SessionOwnershipIndex::build(&OwnershipInputs {
            sessions: &sessions,
            projects: &projects,
            worktrees: &topology,
            ..OwnershipInputs::default()
        });
        let branches = HashMap::new();
        let context = GroupingContext {
            ownership: &ownership,
            spaces: &[],
            surface: Surface::Desktop,
            worktree_sort: WorktreeSortOrder::Manual,
            branches: &branches,
        };
        let refs: Vec<&SessionRecord> = sessions.iter().collect();
        let mut ordering = SessionOrdering::new();
        let section = build_project_section(&project, &refs, &worktrees, &context, &mut ordering);
        let summary: Vec<(String, Vec<&str>)> = section
            .groups
            .iter()
            .map(|group| {
                let ids = group
                    .sessions
                    .iter()
                    .map(|node| node.session.id.as_str())
                    .collect();
                (group.id.clone(), ids)
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("root".to_owned(), vec!["main", "orphan"]),
                ("worktree:/w/feature".to_owned(), vec!["feature"]),
                ("archived".to_owned(), vec!["old"]),
            ]
        );
        assert_eq!(section.groups[0].sessions[0].children.len(), 1);
    }

    #[test]
    fn a_cycle_surfaces_from_its_first_row() {
        let project = ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/p/app".to_owned(),
        };
        let sessions = [child("a", "b", "/p/app"), child("b", "a", "/p/app")];
        let ownership = SessionOwnershipIndex::default();
        let branches = HashMap::new();
        let context = GroupingContext {
            ownership: &ownership,
            spaces: &[],
            surface: Surface::Desktop,
            worktree_sort: WorktreeSortOrder::Manual,
            branches: &branches,
        };
        let refs: Vec<&SessionRecord> = sessions.iter().collect();
        let section =
            build_project_section(&project, &refs, &[], &context, &mut SessionOrdering::new());
        let root = &section.groups[0].sessions;
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].session.id.as_str(), "a");
        assert_eq!(root[0].children[0].session.id.as_str(), "b");
    }
}
