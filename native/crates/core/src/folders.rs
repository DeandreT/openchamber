//! The shared session folders: one user action as a change, applied the
//! same way here and on the server.
//!
//! Ports `packages/ui/src/lib/sessionFolders/folderChanges.ts` (whose
//! semantics the server's `lib/session-folders/folder-changes.js` keeps).
//! A change still pending is laid over every newer server copy the way the
//! server will apply it, so the sidebar shows the user's action at once.

use std::collections::{BTreeMap, HashSet};

use crate::session_actions::{SubmenuAction, SubmenuEntry, SubmenuItem, SubmenuLabel};

/// One folder of a scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionFolder {
    pub id: String,
    pub name: String,
    pub session_ids: Vec<String>,
    pub created_at: i64,
    /// The folder this one sits in.
    pub parent_id: Option<String>,
}

/// Folders by scope key (a project root, worktree or Chats directory).
pub type FoldersMap = BTreeMap<String, Vec<SessionFolder>>;

/// A folder a `Create` change makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewFolder {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub created_at: i64,
}

/// One user action on the shared folders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderChange {
    Create {
        scope: String,
        folder: NewFolder,
    },
    Rename {
        scope: String,
        folder_id: String,
        name: String,
    },
    Delete {
        scope: String,
        folder_ids: Vec<String>,
    },
    Assign {
        scope: String,
        folder_id: String,
        session_ids: Vec<String>,
    },
    Unassign {
        scope: String,
        session_ids: Vec<String>,
    },
    ForgetSessions {
        session_ids: Vec<String>,
    },
    ForgetScopes {
        scopes: Vec<String>,
    },
}

/// A folder and every folder under it.
pub fn folder_subtree(folders: &[SessionFolder], root: &str) -> HashSet<String> {
    let mut result = HashSet::from([root.to_owned()]);
    let mut grew = true;
    while grew {
        grew = false;
        for folder in folders {
            if let Some(parent) = &folder.parent_id
                && result.contains(parent)
                && !result.contains(&folder.id)
            {
                result.insert(folder.id.clone());
                grew = true;
            }
        }
    }
    result
}

fn without_sessions(folders: &mut [SessionFolder], sessions: &HashSet<&str>) -> bool {
    let mut changed = false;
    for folder in folders {
        let before = folder.session_ids.len();
        folder
            .session_ids
            .retain(|id| !sessions.contains(id.as_str()));
        changed |= folder.session_ids.len() != before;
    }
    changed
}

/// Applies one change. Returns whether anything changed, so a caller can
/// skip the write.
pub fn apply_change(map: &mut FoldersMap, change: &FolderChange) -> bool {
    match change {
        FolderChange::Create { scope, folder } => {
            let folders = map.entry(scope.clone()).or_default();
            if folders.iter().any(|known| known.id == folder.id) {
                return false;
            }
            let parent_id = folder
                .parent_id
                .clone()
                .filter(|parent| folders.iter().any(|known| &known.id == parent));
            folders.push(SessionFolder {
                id: folder.id.clone(),
                name: folder.name.clone(),
                session_ids: Vec::new(),
                created_at: folder.created_at,
                parent_id,
            });
            true
        }
        FolderChange::Rename {
            scope,
            folder_id,
            name,
        } => {
            let Some(folder) = map
                .get_mut(scope)
                .and_then(|folders| folders.iter_mut().find(|folder| &folder.id == folder_id))
                .filter(|folder| &folder.name != name)
            else {
                return false;
            };
            folder.name = name.clone();
            true
        }
        FolderChange::Delete { scope, folder_ids } => {
            let Some(folders) = map.get_mut(scope) else {
                return false;
            };
            let mut removed = HashSet::new();
            for id in folder_ids {
                removed.extend(folder_subtree(folders, id));
            }
            let before = folders.len();
            folders.retain(|folder| !removed.contains(&folder.id));
            folders.len() != before
        }
        FolderChange::Assign {
            scope,
            folder_id,
            session_ids,
        } => {
            let Some(folders) = map.get_mut(scope) else {
                return false;
            };
            let Some(target) = folders.iter().position(|folder| &folder.id == folder_id) else {
                return false;
            };
            let moving: HashSet<&str> = session_ids.iter().map(String::as_str).collect();
            let settled = session_ids
                .iter()
                .all(|id| folders[target].session_ids.contains(id))
                && folders.iter().enumerate().all(|(index, folder)| {
                    index == target
                        || !folder
                            .session_ids
                            .iter()
                            .any(|id| moving.contains(id.as_str()))
                });
            if settled {
                return false;
            }
            without_sessions(folders, &moving);
            let mut added = HashSet::new();
            for id in session_ids {
                if added.insert(id) {
                    folders[target].session_ids.push(id.clone());
                }
            }
            true
        }
        FolderChange::Unassign { scope, session_ids } => {
            let Some(folders) = map.get_mut(scope) else {
                return false;
            };
            let sessions: HashSet<&str> = session_ids.iter().map(String::as_str).collect();
            without_sessions(folders, &sessions)
        }
        FolderChange::ForgetSessions { session_ids } => {
            let sessions: HashSet<&str> = session_ids.iter().map(String::as_str).collect();
            let mut changed = false;
            for folders in map.values_mut() {
                changed |= without_sessions(folders, &sessions);
            }
            changed
        }
        FolderChange::ForgetScopes { scopes } => {
            let mut changed = false;
            for scope in scopes {
                changed |= map.remove(scope).is_some();
            }
            changed
        }
    }
}

/// Applies changes in order.
pub fn apply_changes<'c>(
    map: &mut FoldersMap,
    changes: impl IntoIterator<Item = &'c FolderChange>,
) {
    for change in changes {
        apply_change(map, change);
    }
}

/// The folder of `scope` holding `session`.
pub fn session_folder<'m>(
    map: &'m FoldersMap,
    scope: &str,
    session: &str,
) -> Option<&'m SessionFolder> {
    map.get(scope)?
        .iter()
        .find(|folder| folder.session_ids.iter().any(|id| id == session))
}

/// The scopes a session can be filed in (`SessionNodeItem`'s Move to
/// folder): folders are flat per project, so the project's root, each of
/// its worktrees, the Chats root of a chat and the session's own directory.
pub fn folder_scopes<'s>(
    project_root: Option<&'s str>,
    worktrees: impl IntoIterator<Item = &'s str>,
    chats_root: Option<&'s str>,
    session_directory: &'s str,
) -> Vec<String> {
    let mut scopes: Vec<String> = Vec::new();
    let mut push = |candidate: Option<&str>| {
        if let Some(scope) = candidate.and_then(crate::path::normalize)
            && !scopes.contains(&scope)
        {
            scopes.push(scope);
        }
    };
    push(project_root);
    if project_root.is_some() {
        for worktree in worktrees {
            push(Some(worktree));
        }
    }
    push(chats_root);
    push(Some(session_directory));
    scopes
}

/// The Move to folder submenu: every folder of the scopes (the current one
/// checked), New folder after a separator, and Remove from folder while
/// filed. A scope's folders list in their stored order.
pub fn folder_submenu(scopes: &[String], map: &FoldersMap, session: &str) -> Vec<SubmenuEntry> {
    let mut entries = Vec::new();
    let mut current = None;
    for scope in scopes {
        for folder in map.get(scope).into_iter().flatten() {
            let checked = folder.session_ids.iter().any(|id| id == session);
            if checked && current.is_none() {
                current = Some(scope.clone());
            }
            entries.push(SubmenuEntry::Item(SubmenuItem {
                checked,
                ..SubmenuItem::new(
                    SubmenuAction::AssignFolder {
                        scope: scope.clone(),
                        folder_id: folder.id.clone(),
                    },
                    SubmenuLabel::Text(folder.name.clone()),
                )
            }));
        }
    }
    if entries.is_empty() {
        entries.push(SubmenuEntry::Note("sessions.sidebar.folders.none"));
    }
    entries.push(SubmenuEntry::Separator);
    if let Some(default_scope) = scopes.first() {
        entries.push(SubmenuEntry::Item(SubmenuItem {
            icon: Some("add"),
            ..SubmenuItem::new(
                SubmenuAction::NewFolder {
                    scope: default_scope.clone(),
                },
                SubmenuLabel::Key("sessions.sidebar.folders.newFolderEllipsis"),
            )
        }));
    }
    if let Some(scope) = current {
        entries.push(SubmenuEntry::Item(SubmenuItem {
            icon: Some("close"),
            destructive: true,
            ..SubmenuItem::new(
                SubmenuAction::RemoveFromFolder { scope },
                SubmenuLabel::Key("sessions.sidebar.folders.removeFromFolder"),
            )
        }));
    }
    entries
}

/// The changes filing `session` in `folder_id` of `scope` makes: it leaves
/// every other scope's folder first, as the reference's menu does. Choosing
/// the folder it is in takes it out instead.
pub fn file_session(
    scopes: &[String],
    map: &FoldersMap,
    scope: &str,
    folder_id: &str,
    session: &str,
) -> Vec<FolderChange> {
    let sessions = vec![session.to_owned()];
    if session_folder(map, scope, session).is_some_and(|folder| folder.id == folder_id) {
        return vec![FolderChange::Unassign {
            scope: scope.to_owned(),
            session_ids: sessions,
        }];
    }
    let mut changes: Vec<FolderChange> = scopes
        .iter()
        .filter(|candidate| candidate.as_str() != scope)
        .map(|candidate| FolderChange::Unassign {
            scope: candidate.clone(),
            session_ids: sessions.clone(),
        })
        .collect();
    changes.push(FolderChange::Assign {
        scope: scope.to_owned(),
        folder_id: folder_id.to_owned(),
        session_ids: sessions,
    });
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(scope: &str, id: &str, parent: Option<&str>) -> FolderChange {
        FolderChange::Create {
            scope: scope.to_owned(),
            folder: NewFolder {
                id: id.to_owned(),
                name: format!("Folder {id}"),
                parent_id: parent.map(str::to_owned),
                created_at: 1,
            },
        }
    }

    fn assign(scope: &str, folder: &str, sessions: &[&str]) -> FolderChange {
        FolderChange::Assign {
            scope: scope.to_owned(),
            folder_id: folder.to_owned(),
            session_ids: sessions.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    #[test]
    fn creating_twice_or_under_a_missing_parent_keeps_one_root_folder() {
        let mut map = FoldersMap::new();
        assert!(apply_change(&mut map, &create("/p", "a", Some("missing"))));
        assert!(!apply_change(&mut map, &create("/p", "a", None)));
        assert_eq!(map["/p"].len(), 1);
        assert_eq!(map["/p"][0].parent_id, None);
    }

    #[test]
    fn assigning_moves_a_session_out_of_its_other_folder() {
        let mut map = FoldersMap::new();
        apply_changes(
            &mut map,
            &[
                create("/p", "a", None),
                create("/p", "b", None),
                assign("/p", "a", &["s1", "s2"]),
            ],
        );
        assert!(apply_change(&mut map, &assign("/p", "b", &["s1"])));
        assert_eq!(map["/p"][0].session_ids, ["s2"]);
        assert_eq!(map["/p"][1].session_ids, ["s1"]);
        assert!(
            !apply_change(&mut map, &assign("/p", "b", &["s1"])),
            "a settled assignment changes nothing"
        );
        assert_eq!(
            session_folder(&map, "/p", "s1").map(|folder| folder.id.as_str()),
            Some("b")
        );
    }

    #[test]
    fn deleting_a_folder_takes_its_subfolders() {
        let mut map = FoldersMap::new();
        apply_changes(
            &mut map,
            &[
                create("/p", "a", None),
                create("/p", "b", Some("a")),
                create("/p", "c", None),
            ],
        );
        assert!(apply_change(
            &mut map,
            &FolderChange::Delete {
                scope: "/p".to_owned(),
                folder_ids: vec!["a".to_owned()],
            }
        ));
        let ids: Vec<&str> = map["/p"].iter().map(|folder| folder.id.as_str()).collect();
        assert_eq!(ids, ["c"]);
    }

    #[test]
    fn forgetting_sessions_and_scopes_reaches_every_scope() {
        let mut map = FoldersMap::new();
        apply_changes(
            &mut map,
            &[
                create("/p", "a", None),
                create("/q", "b", None),
                assign("/p", "a", &["s"]),
                assign("/q", "b", &["s"]),
            ],
        );
        assert!(apply_change(
            &mut map,
            &FolderChange::ForgetSessions {
                session_ids: vec!["s".to_owned()],
            }
        ));
        assert!(
            map.values()
                .flatten()
                .all(|folder| folder.session_ids.is_empty())
        );
        assert!(apply_change(
            &mut map,
            &FolderChange::ForgetScopes {
                scopes: vec!["/q".to_owned()],
            }
        ));
        assert!(!map.contains_key("/q"));
    }

    #[test]
    fn renaming_to_the_same_name_changes_nothing() {
        let mut map = FoldersMap::new();
        apply_change(&mut map, &create("/p", "a", None));
        let rename = |name: &str| FolderChange::Rename {
            scope: "/p".to_owned(),
            folder_id: "a".to_owned(),
            name: name.to_owned(),
        };
        assert!(!apply_change(&mut map, &rename("Folder a")));
        assert!(apply_change(&mut map, &rename("Docs")));
        assert_eq!(map["/p"][0].name, "Docs");
    }

    #[test]
    fn the_submenu_lists_every_scope_and_checks_the_current_folder() {
        let mut map = FoldersMap::new();
        apply_changes(
            &mut map,
            &[
                create("/p", "a", None),
                create("/p/.worktrees/w", "b", None),
                assign("/p/.worktrees/w", "b", &["s"]),
            ],
        );
        let scopes = folder_scopes(Some("/p"), ["/p/.worktrees/w"], None, "/p/.worktrees/w");
        assert_eq!(scopes, ["/p", "/p/.worktrees/w"]);
        let entries = folder_submenu(&scopes, &map, "s");
        let checked: Vec<bool> = entries
            .iter()
            .filter_map(|entry| match entry {
                SubmenuEntry::Item(item) => Some(item.checked),
                _ => None,
            })
            .collect();
        assert_eq!(checked, [false, true, false, false]);
        assert_eq!(
            entries.last().and_then(SubmenuEntry::action),
            Some(&SubmenuAction::RemoveFromFolder {
                scope: "/p/.worktrees/w".to_owned()
            })
        );
        let empty = folder_submenu(&scopes, &FoldersMap::new(), "s");
        assert_eq!(
            empty[0],
            SubmenuEntry::Note("sessions.sidebar.folders.none")
        );
    }

    #[test]
    fn filing_leaves_other_scopes_and_the_current_folder_toggles_out() {
        let mut map = FoldersMap::new();
        apply_changes(
            &mut map,
            &[
                create("/p", "a", None),
                create("/w", "b", None),
                assign("/w", "b", &["s"]),
            ],
        );
        let scopes = vec!["/p".to_owned(), "/w".to_owned()];
        let changes = file_session(&scopes, &map, "/p", "a", "s");
        apply_changes(&mut map, &changes);
        assert_eq!(
            session_folder(&map, "/p", "s").map(|f| f.id.as_str()),
            Some("a")
        );
        assert!(session_folder(&map, "/w", "s").is_none());
        let again = file_session(&scopes, &map, "/p", "a", "s");
        apply_changes(&mut map, &again);
        assert!(session_folder(&map, "/p", "s").is_none());
    }
}
