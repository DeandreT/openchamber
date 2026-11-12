//! The new-session draft's Chat target: a session that belongs to no project
//! and lives in its own folder under the server's managed Chats root.
//!
//! Ports the target rules of `openNewSessionDraft` and
//! `setNewSessionDraftTarget` (`sync/session-ui-store.ts`), the folder name of
//! `createChatDirectory` (`lib/chatDirectories.ts`), the Chat step of
//! `pickEffectiveDirectory` (`hooks/useEffectiveDirectory.ts`) and the text a
//! moved draft carries (`composer/state/useComposerDraft.ts`).

use crate::chat_roots::ChatRoots;
use crate::path;

/// The project identity a Chat creation records (`CHAT_DRAFT_PROJECT_ID`).
/// No registered project can have it: project identities are not URIs.
pub const CHAT_PROJECT_ID: &str = "openchamber:chats";

/// The draft identity of the one open Chat draft of a runtime.
pub const CHAT_DRAFT_ID: &str = "chat-new-session";

/// Where a new-session draft will create its session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftDestination {
    /// A managed Chat, in a folder of its own under the Chats root.
    Chat,
    /// A registered project root or one of its worktrees.
    Project(String),
}

/// What an entry point asked for when it opened a new-session draft.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenRequest<'a> {
    /// The Chats header's "+": a Chat whatever was targeted last.
    pub chat: bool,
    /// `directoryOverride`: the directory the entry point forwards, such as
    /// the selected session's for the shortcut and the menu.
    pub directory: Option<&'a str>,
    /// The side of the target picker the user last worked on.
    pub last: Option<&'a DraftDestination>,
}

/// The destination a new draft opens on.
///
/// An explicit directory names a project draft unless it is a managed Chat
/// folder, which asks for another Chat. With nothing explicit the draft
/// reopens on the last project target while that project is still
/// registered, and on Chat otherwise. `registered` answers whether a session
/// can be created in a directory.
///
/// The reference opens a project draft for any explicit directory. Native
/// creates sessions only in registered directories, so an unregistered one
/// opens a Chat, as the reference does for an unregistered live directory.
pub fn resolve_destination(
    request: OpenRequest<'_>,
    roots: Option<&ChatRoots>,
    registered: impl Fn(&str) -> bool,
) -> DraftDestination {
    if request.chat {
        return DraftDestination::Chat;
    }
    if let Some(directory) = request.directory {
        return match path::normalize(directory) {
            Some(directory)
                if !roots.is_some_and(|roots| roots.contains(Some(directory.as_str())))
                    && registered(&directory) =>
            {
                DraftDestination::Project(directory)
            }
            _ => DraftDestination::Chat,
        };
    }
    match request.last {
        Some(DraftDestination::Project(directory)) if registered(directory) => {
            DraftDestination::Project(directory.clone())
        }
        _ => DraftDestination::Chat,
    }
}

/// How far the read of the server's Chats root got.
///
/// A failed read is never the same as a server without a root: neither can
/// create a Chat, but only the failure is worth reading again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootsStatus {
    /// No answer yet.
    Pending,
    /// The server named its root.
    Ready,
    /// The server answered without a usable home, so it has no root.
    Missing,
    /// The last read failed.
    Failed,
}

impl RootsStatus {
    /// `answered` is whether a read completed, `failed` whether the last
    /// one gave up.
    pub fn of(roots: Option<&ChatRoots>, answered: bool, failed: bool) -> Self {
        match (roots, answered, failed) {
            (Some(_), _, _) => Self::Ready,
            (None, true, _) => Self::Missing,
            (None, false, true) => Self::Failed,
            (None, false, false) => Self::Pending,
        }
    }
}

/// A calendar day, for the dated folder a Chat is created under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChatDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

/// The folder of one new Chat: `<root>/<YYYY-MM-DD>/session-<id>`.
///
/// Returns `None` for an `id` that is empty or would leave the dated folder.
pub fn session_directory(roots: &ChatRoots, date: ChatDate, id: &str) -> Option<String> {
    let usable = !id.is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-');
    usable.then(|| {
        format!(
            "{}/{:04}-{:02}-{:02}/session-{id}",
            roots.configured().trim_end_matches('/'),
            date.year,
            date.month,
            date.day
        )
    })
}

/// The directory the panels, the catalogs and the terminal of a Chat draft
/// work in: the managed Chats root, and nothing until the server names it.
/// Never the project that was on screen before, whose files, commands and
/// skills would leak into the chat.
pub fn effective_directory(roots: Option<&ChatRoots>) -> Option<String> {
    roots.and_then(|roots| path::normalize(roots.configured()))
}

/// Whether `folder` can still hold a new Chat: a descendant of a managed
/// root, never a root itself.
pub fn is_session_folder(roots: &ChatRoots, folder: &str) -> bool {
    roots
        .root_of(Some(folder))
        .is_some_and(|root| path::normalize(folder).as_deref() != Some(root))
}

/// The text of a draft that moved to another destination: what was typed
/// follows what the destination already kept, and equal texts stay single.
pub fn carried_text(saved: &str, outgoing: &str) -> String {
    if saved.is_empty() || saved == outgoing {
        outgoing.to_owned()
    } else if outgoing.is_empty() {
        saved.to_owned()
    } else {
        format!("{saved}\n\n{outgoing}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> ChatRoots {
        ChatRoots::from_home_info(Some("/home/me"), Some("/data/chats"), None, None)
            .expect("home is usable")
    }

    fn registered(directory: &str) -> bool {
        matches!(directory, "/work/app" | "/work/app-worktree")
    }

    #[test]
    fn a_plain_new_session_opens_a_chat_until_a_project_was_targeted_last() {
        let roots = roots();
        assert_eq!(
            resolve_destination(OpenRequest::default(), Some(&roots), registered),
            DraftDestination::Chat
        );
        let project = DraftDestination::Project("/work/app".to_owned());
        assert_eq!(
            resolve_destination(
                OpenRequest {
                    last: Some(&project),
                    ..OpenRequest::default()
                },
                Some(&roots),
                registered,
            ),
            project
        );
        assert_eq!(
            resolve_destination(
                OpenRequest {
                    last: Some(&DraftDestination::Chat),
                    ..OpenRequest::default()
                },
                Some(&roots),
                registered,
            ),
            DraftDestination::Chat
        );
    }

    #[test]
    fn a_project_removed_since_it_was_targeted_reopens_as_a_chat() {
        let removed = DraftDestination::Project("/work/gone".to_owned());
        assert_eq!(
            resolve_destination(
                OpenRequest {
                    last: Some(&removed),
                    ..OpenRequest::default()
                },
                Some(&roots()),
                registered,
            ),
            DraftDestination::Chat
        );
    }

    #[test]
    fn a_forwarded_session_directory_names_its_project_or_another_chat() {
        let roots = roots();
        let last = DraftDestination::Project("/work/app".to_owned());
        let open = |directory| {
            resolve_destination(
                OpenRequest {
                    directory: Some(directory),
                    last: Some(&last),
                    ..OpenRequest::default()
                },
                Some(&roots),
                registered,
            )
        };
        assert_eq!(
            open("/work/app-worktree/"),
            DraftDestination::Project("/work/app-worktree".to_owned())
        );
        // A chat session's folder owns no project, and the last project
        // target does not take the draft back either.
        assert_eq!(
            open("/data/chats/2026-11-11/session-a"),
            DraftDestination::Chat
        );
        assert_eq!(
            open("/home/me/.config/openchamber/chats/old"),
            DraftDestination::Chat
        );
        assert_eq!(open("/elsewhere"), DraftDestination::Chat);
    }

    #[test]
    fn the_chats_header_opens_a_chat_over_every_other_input() {
        let last = DraftDestination::Project("/work/app".to_owned());
        assert_eq!(
            resolve_destination(
                OpenRequest {
                    chat: true,
                    directory: Some("/work/app"),
                    last: Some(&last),
                },
                Some(&roots()),
                registered,
            ),
            DraftDestination::Chat
        );
    }

    #[test]
    fn a_failed_root_read_is_not_a_server_without_a_root() {
        let roots = roots();
        assert_eq!(
            RootsStatus::of(Some(&roots), true, false),
            RootsStatus::Ready
        );
        assert_eq!(RootsStatus::of(None, true, false), RootsStatus::Missing);
        assert_eq!(RootsStatus::of(None, false, true), RootsStatus::Failed);
        assert_eq!(RootsStatus::of(None, false, false), RootsStatus::Pending);
        // Roots kept from an earlier answer outlive a later failed refresh.
        assert_eq!(
            RootsStatus::of(Some(&roots), false, true),
            RootsStatus::Ready
        );
    }

    #[test]
    fn a_chat_is_created_in_a_dated_folder_of_its_own_under_the_root() {
        let roots = roots();
        let date = ChatDate {
            year: 2026,
            month: 3,
            day: 7,
        };
        let folder = session_directory(&roots, date, "0a1b-2c").expect("the id is usable");
        assert_eq!(folder, "/data/chats/2026-03-07/session-0a1b-2c");
        assert!(roots.contains(Some(folder.as_str())));
        assert!(is_session_folder(&roots, &folder));
        assert_eq!(session_directory(&roots, date, ""), None);
        assert_eq!(session_directory(&roots, date, "../escape"), None);
        assert_eq!(session_directory(&roots, date, "a/b"), None);
    }

    #[test]
    fn a_session_created_in_the_chat_folder_is_listed_under_chats() {
        use crate::ids::ProjectId;
        use crate::ordering::SessionOrdering;
        use crate::ownership::{ConfiguredProject, OwnershipInputs};
        use crate::session::SessionRecord;
        use crate::sidebar_collection::SidebarCollection;

        let roots = roots();
        let date = ChatDate {
            year: 2026,
            month: 11,
            day: 11,
        };
        let folder = session_directory(&roots, date, "abc").expect("the id is usable");
        let projects = [ConfiguredProject {
            id: ProjectId::from("app"),
            path: "/work/app".into(),
        }];
        let sessions = [
            SessionRecord::new("chat", &folder),
            SessionRecord::new("project", "/work/app"),
        ];
        let collection = SidebarCollection::build(
            &OwnershipInputs {
                sessions: &sessions,
                projects: &projects,
                ..OwnershipInputs::default()
            },
            Some(&roots),
            &mut SessionOrdering::new(),
        );
        let chats: Vec<&str> = collection
            .chat_sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        let owned: Vec<&str> = collection
            .project_sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        assert_eq!(chats, ["chat"]);
        assert_eq!(owned, ["project"]);
    }

    #[test]
    fn a_root_or_a_folder_outside_the_roots_holds_no_new_chat() {
        let roots = roots();
        assert!(!is_session_folder(&roots, "/data/chats"));
        assert!(!is_session_folder(&roots, "/data/chats/"));
        assert!(!is_session_folder(&roots, "/work/app"));
        assert!(is_session_folder(
            &roots,
            "/home/me/.config/openchamber/chats/2025-01-01/session-a"
        ));
    }

    #[test]
    fn a_chat_draft_works_in_the_root_and_nowhere_before_it_is_known() {
        assert_eq!(effective_directory(None), None);
        assert_eq!(
            effective_directory(Some(&roots())).as_deref(),
            Some("/data/chats")
        );
    }

    #[test]
    fn a_moved_draft_keeps_its_text_after_what_the_destination_held() {
        assert_eq!(carried_text("", "typed"), "typed");
        assert_eq!(carried_text("kept", "typed"), "kept\n\ntyped");
        assert_eq!(carried_text("same", "same"), "same");
        assert_eq!(carried_text("kept", ""), "kept");
    }
}
