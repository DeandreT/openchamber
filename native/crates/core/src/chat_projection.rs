//! Chats membership: managed chats plus unowned root conversations.
//!
//! Mirrors `projectSidebarChats` in
//! `packages/ui/src/components/session/sidebar/sessions/sessionChatProjection.ts`.
//! Membership is display-only; records and ownership stay unchanged.

use std::collections::{HashMap, HashSet};

use crate::chat_roots::{self, ChatRoots};
use crate::ids::{ProjectId, SessionId};
use crate::ownership::{SessionOwnershipIndex, Surface};
use crate::session::SessionRecord;

/// How sessions divide between Chats and projects.
#[derive(Debug, Default)]
pub struct ChatProjection<'a> {
    /// Chats members in input order: managed chats, unowned roots and their
    /// active descendants.
    pub chat_sessions: Vec<&'a SessionRecord>,
    /// Sessions that keep a project entry. Owned descendants of a fallback root
    /// stay here because Chats rows do not expand.
    pub project_sessions: Vec<&'a SessionRecord>,
    /// Owned sessions per project, without roots that moved into Chats.
    pub project_sessions_by_project: HashMap<ProjectId, Vec<&'a SessionRecord>>,
}

impl ChatProjection<'_> {
    pub fn is_chat_member(&self, session_id: &SessionId) -> bool {
        self.chat_sessions
            .iter()
            .any(|session| &session.id == session_id)
    }
}

/// Divides active sessions between Chats and projects.
///
/// A root conversation stays in Chats until configured topology or canonical
/// metadata resolves an owner. Its active descendants follow it across
/// directories. Children whose parent is missing, archived or a `/btw` fork
/// are never promoted, and cycles without a root are never reached. VS Code
/// has no Chats fallback.
pub fn project<'a>(
    sessions: &'a [SessionRecord],
    ownership: &SessionOwnershipIndex<'a>,
    roots: Option<&ChatRoots>,
    surface: Surface,
) -> ChatProjection<'a> {
    let is_chat_directory = |session: &SessionRecord| {
        chat_roots::is_chat_directory(roots, session.directory.as_deref())
    };
    let active: Vec<&'a SessionRecord> = sessions
        .iter()
        .filter(|session| !session.is_archived() && !session.is_btw())
        .collect();

    let mut members: HashSet<&'a SessionId> = HashSet::new();
    if surface == Surface::Desktop {
        let mut children: HashMap<&'a SessionId, Vec<&'a SessionRecord>> = HashMap::new();
        let mut pending: Vec<&'a SessionRecord> = Vec::new();
        for session in &active {
            match &session.parent_id {
                Some(parent_id) => children.entry(parent_id).or_default().push(session),
                None if is_chat_directory(session) || !ownership.is_owned(&session.id) => {
                    pending.push(session);
                }
                None => {}
            }
        }
        while let Some(session) = pending.pop() {
            if !members.insert(&session.id) {
                continue;
            }
            if let Some(descendants) = children.get(&session.id) {
                pending.extend(descendants);
            }
        }
    }

    let is_chat_root = |session: &SessionRecord| session.is_root() && members.contains(&session.id);
    let project_sessions_by_project = ownership
        .sessions_by_project()
        .iter()
        .map(|(project_id, bucket)| {
            let kept: Vec<&'a SessionRecord> = bucket
                .iter()
                .copied()
                .filter(|session| !is_chat_root(session))
                .collect();
            (project_id.clone(), kept)
        })
        .collect();

    let chat_sessions = active
        .iter()
        .copied()
        .filter(|session| members.contains(&session.id))
        .collect();
    let project_sessions = active
        .iter()
        .copied()
        .filter(|session| {
            !is_chat_directory(session)
                && (!members.contains(&session.id) || ownership.is_owned(&session.id))
        })
        .collect();

    ChatProjection {
        chat_sessions,
        project_sessions,
        project_sessions_by_project,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ownership::{ConfiguredProject, OwnershipInputs};
    use crate::session::SessionKind;

    fn ids(sessions: &[&SessionRecord]) -> Vec<String> {
        sessions
            .iter()
            .map(|session| session.id.as_str().to_owned())
            .collect()
    }

    fn child(id: &str, parent: &str, directory: &str) -> SessionRecord {
        let mut session = SessionRecord::new(id, directory);
        session.parent_id = Some(SessionId::from(parent));
        session
    }

    #[test]
    fn unowned_roots_carry_descendants_but_not_orphans() {
        let mut btw = SessionRecord::new("btw", "/tmp/a");
        btw.kind = SessionKind::Btw;
        let mut archived = SessionRecord::new("archived", "/tmp/a");
        archived.time.archived = Some(1);
        let sessions = vec![
            SessionRecord::new("root", "/tmp/a"),
            child("child", "root", "/projects/app"),
            child("grandchild", "child", "/tmp/b"),
            btw,
            child("btw-child", "btw", "/tmp/a"),
            archived,
            child("archived-child", "archived", "/tmp/a"),
            child("orphan", "missing", "/tmp/a"),
            child("cycle-a", "cycle-b", "/tmp/a"),
            child("cycle-b", "cycle-a", "/tmp/a"),
        ];
        let projects = [ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/projects/app".to_owned(),
        }];
        let ownership = SessionOwnershipIndex::build(&OwnershipInputs {
            sessions: &sessions,
            projects: &projects,
            ..OwnershipInputs::default()
        });
        let projection = project(&sessions, &ownership, None, Surface::Desktop);
        assert_eq!(
            ids(&projection.chat_sessions),
            ["root", "child", "grandchild"]
        );
        // The owned child keeps its project entry; unowned members leave projects.
        assert_eq!(
            ids(&projection.project_sessions),
            [
                "child",
                "btw-child",
                "archived-child",
                "orphan",
                "cycle-a",
                "cycle-b"
            ]
        );
    }

    #[test]
    fn vs_code_has_no_fallback() {
        let sessions = vec![SessionRecord::new("root", "/tmp/a")];
        let ownership = SessionOwnershipIndex::build(&OwnershipInputs {
            sessions: &sessions,
            surface: Surface::VsCode,
            ..OwnershipInputs::default()
        });
        let projection = project(&sessions, &ownership, None, Surface::VsCode);
        assert!(projection.chat_sessions.is_empty());
        assert_eq!(ids(&projection.project_sessions), ["root"]);
    }
}
