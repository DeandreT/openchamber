//! Session visibility before sidebar ownership and grouping.
//!
//! Mirrors the production collection in `sessionCollection.ts` and registered
//! topology in `sessionListDirectories.ts`. Filtering never mutates the global
//! index or a session's routing directory.

use std::collections::HashSet;

use crate::chat_roots::{self, ChatRoots};
use crate::ordering::SessionOrdering;
use crate::ownership::{OwnershipInputs, Surface};
use crate::path;
use crate::permission_mode;
use crate::session::SessionRecord;

/// Borrowed active and archived inputs admitted to the sidebar.
#[derive(Debug, Default)]
pub struct SidebarCollection<'a> {
    /// Combined admitted hierarchy in lifecycle order, shared by all row trees.
    pub ordered_sessions: Vec<&'a SessionRecord>,
    /// Active managed Chats in lifecycle order.
    pub chat_sessions: Vec<&'a SessionRecord>,
    /// Active registered project, worktree and isolated-space sessions.
    pub project_sessions: Vec<&'a SessionRecord>,
    /// Scoped archive records, including managed Chats and temporary forks.
    pub archived_sessions: Vec<&'a SessionRecord>,
}

impl<'a> SidebarCollection<'a> {
    /// Filters the complete global index using the selected runtime's topology.
    ///
    /// Unregistered directories await admission. Worktrees are admitted only
    /// under registered projects; VS Code admits exact workspace paths alone.
    pub fn build(
        inputs: &OwnershipInputs<'a>,
        roots: Option<&ChatRoots>,
        ordering: &mut SessionOrdering,
    ) -> Self {
        let registered: HashSet<String> = inputs
            .projects
            .iter()
            .filter_map(|project| path::normalize(&project.path))
            .collect();
        let mut known = registered.clone();
        if inputs.surface == Surface::Desktop {
            for project in inputs.worktrees {
                let registered_owner = path::normalize(&project.project_path)
                    .is_some_and(|root| registered.contains(&root));
                if !registered_owner {
                    continue;
                }
                for worktree in &project.worktrees {
                    if let Some(directory) = path::normalize(&worktree.path) {
                        known.insert(directory);
                    }
                }
            }
        } else {
            known = known
                .into_iter()
                .map(|directory| directory.to_lowercase())
                .collect();
        }
        let is_known = |session: &SessionRecord| {
            let Some(mut directory) = session.resolved_directory() else {
                return false;
            };
            if inputs.surface == Surface::VsCode {
                directory = directory.to_lowercase();
                return known.contains(&directory);
            }
            permission_mode::is_space_directory(&directory)
                || known.contains(&directory)
                || known.iter().any(|root| {
                    directory
                        .strip_prefix(root)
                        .is_some_and(|rest| root.ends_with('/') || rest.starts_with('/'))
                })
        };
        let is_chat = |session: &SessionRecord| {
            chat_roots::is_chat_directory(roots, session.directory.as_deref())
        };
        let mut collection = Self::default();
        for session in inputs.sessions {
            if session.is_archived() || session.is_btw() {
                continue;
            }
            if is_chat(session) {
                if inputs.surface == Surface::Desktop {
                    collection.chat_sessions.push(session);
                }
            } else if is_known(session) {
                collection.project_sessions.push(session);
            }
        }
        for session in inputs.archived_sessions {
            let visible = if is_chat(session) {
                inputs.surface == Surface::Desktop
            } else {
                is_known(session)
            };
            if visible {
                collection.archived_sessions.push(session);
            }
        }
        let chat_ids: HashSet<_> = collection
            .chat_sessions
            .iter()
            .map(|session| &session.id)
            .collect();
        let sessions: Vec<_> = collection
            .project_sessions
            .iter()
            .chain(&collection.chat_sessions)
            .copied()
            .collect();
        collection.ordered_sessions = ordering.order_scopes(&sessions);
        (collection.chat_sessions, collection.project_sessions) = collection
            .ordered_sessions
            .iter()
            .copied()
            .partition(|session| chat_ids.contains(&session.id));
        collection
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ProjectId;
    use crate::ownership::{ConfiguredProject, ProjectWorktrees, WorktreeInfo};
    use crate::session::SessionKind;

    fn ids<'a>(sessions: &[&'a SessionRecord]) -> Vec<&'a str> {
        sessions.iter().map(|session| session.id.as_str()).collect()
    }

    #[test]
    fn registration_gates_membership_without_changing_source_records() {
        let roots = ChatRoots::from_home_info(Some("/home/test"), None, None, None)
            .expect("synthetic home supplies managed roots");
        let projects = [ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/app".into(),
        }];
        let worktrees = [
            ProjectWorktrees {
                project_path: "/app".into(),
                worktrees: vec![WorktreeInfo::new("/linked")],
            },
            ProjectWorktrees {
                project_path: "/unregistered".into(),
                worktrees: vec![WorktreeInfo::new("/other")],
            },
        ];
        let sessions = [
            SessionRecord::new("managed", "/home/test/.config/openchamber/chats/thread"),
            SessionRecord::new("unregistered", "/tmp/scratch"),
            SessionRecord::new("nested", "/app/src"),
            SessionRecord::new("linked", "/linked/src"),
            SessionRecord::new("other", "/other"),
            SessionRecord::new("space", "/spaces/0123456789ab/repo"),
            SessionRecord::new("invalid-space", "/spaces/other/repo"),
            SessionRecord::new("sibling", "/app-other"),
        ];
        let original = sessions.clone();
        let collection = SidebarCollection::build(
            &OwnershipInputs {
                sessions: &sessions,
                projects: &projects,
                worktrees: &worktrees,
                ..OwnershipInputs::default()
            },
            Some(&roots),
            &mut SessionOrdering::new(),
        );
        assert_eq!(ids(&collection.chat_sessions), ["managed"]);
        assert_eq!(
            ids(&collection.project_sessions),
            ["linked", "nested", "space"]
        );
        assert_eq!(
            sessions, original,
            "visibility changes no routing or source state"
        );
    }

    #[test]
    fn active_forks_are_hidden_while_scoped_archives_retain_them() {
        let mut fork = SessionRecord::new("fork", "/app");
        fork.kind = SessionKind::Btw;
        let active = [fork.clone()];
        let archives = [fork, SessionRecord::new("outside", "/tmp/scratch")];
        let projects = [ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/app".into(),
        }];
        let collection = SidebarCollection::build(
            &OwnershipInputs {
                sessions: &active,
                archived_sessions: &archives,
                projects: &projects,
                ..OwnershipInputs::default()
            },
            None,
            &mut SessionOrdering::new(),
        );
        assert!(collection.project_sessions.is_empty());
        assert_eq!(ids(&collection.archived_sessions), ["fork"]);
    }
}
