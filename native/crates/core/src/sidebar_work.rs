//! Sidebar work membership and order, matching `works/sidebarWorks.ts`.
//!
//! A linked worktree or isolated space groups all its sessions into one work.
//! Elsewhere, each root session owns the sessions nested beneath it. Archived
//! groups do not contribute work. Ordered session input determines work order.

use std::collections::{HashMap, HashSet};

use crate::grouping::{GroupLabel, ProjectSection, SessionGroup, SessionNode};
use crate::ids::{ProjectId, SessionId};
use crate::session::SessionRecord;

/// One item in a project's Works list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarWork {
    Session {
        project_id: ProjectId,
        root_session_id: SessionId,
    },
    Group {
        project_id: ProjectId,
        group_id: String,
    },
}

impl SidebarWork {
    /// The stable key used by the web sidebar's work index.
    pub fn key(&self) -> String {
        match self {
            Self::Session {
                root_session_id, ..
            } => format!("session:{root_session_id}"),
            Self::Group {
                project_id,
                group_id,
            } => format!("{project_id}:{group_id}"),
        }
    }
}

/// Work ownership of every listed session, with empty group works retained.
#[derive(Clone, Debug, Default)]
pub struct SidebarWorkIndex {
    works: Vec<SidebarWork>,
    by_session: HashMap<SessionId, usize>,
    groups: Vec<usize>,
}

impl SidebarWorkIndex {
    /// Builds the index in project and group order. The first owner of a
    /// duplicate session keeps it, matching the web projection.
    pub fn new(sections: &[ProjectSection<'_>]) -> Self {
        let mut index = Self::default();
        for section in sections {
            for group in &section.groups {
                if group.is_archived_bucket {
                    continue;
                }
                if is_group_work(group) {
                    let work = index.works.len();
                    index.works.push(SidebarWork::Group {
                        project_id: section.project.id.clone(),
                        group_id: group.id.clone(),
                    });
                    index.groups.push(work);
                    for node in &group.sessions {
                        index.claim(node, work);
                    }
                } else {
                    for node in &group.sessions {
                        let work = index.works.len();
                        index.works.push(SidebarWork::Session {
                            project_id: section.project.id.clone(),
                            root_session_id: node.session.id.clone(),
                        });
                        index.claim(node, work);
                    }
                }
            }
        }
        index
    }

    fn claim(&mut self, node: &SessionNode<'_>, work: usize) {
        use std::collections::hash_map::Entry;
        let mut pending = vec![node];
        while let Some(node) = pending.pop() {
            let Entry::Vacant(entry) = self.by_session.entry(node.session.id.clone()) else {
                continue;
            };
            entry.insert(work);
            pending.extend(&node.children);
        }
    }

    /// The work containing a session, when its group participates in Works.
    pub fn work_for(&self, session_id: &SessionId) -> Option<&SidebarWork> {
        self.by_session
            .get(session_id)
            .map(|index| &self.works[*index])
    }

    /// Every linked worktree and isolated space in section order, even empty ones.
    pub fn groups(&self) -> impl Iterator<Item = &SidebarWork> {
        self.groups.iter().map(|index| &self.works[*index])
    }

    /// Each work once, ordered by the first session `include` accepts.
    /// Callers provide the authoritative lifecycle order and zone predicate.
    pub fn ordered<'a>(
        &'a self,
        sessions: &[&SessionRecord],
        mut include: impl FnMut(&SessionRecord) -> bool,
    ) -> Vec<&'a SidebarWork> {
        let mut seen = HashSet::new();
        let mut works = Vec::new();
        for session in sessions {
            if !include(session) {
                continue;
            }
            let Some(index) = self.by_session.get(&session.id) else {
                continue;
            };
            if seen.insert(*index) {
                works.push(&self.works[*index]);
            }
        }
        works
    }
}

/// Linked worktrees and isolated spaces form a work of their own.
pub fn is_group_work(group: &SessionGroup<'_>) -> bool {
    !group.is_main
        && !group.is_archived_bucket
        && (group.space.is_some() || matches!(group.label, GroupLabel::Worktree { .. }))
}
