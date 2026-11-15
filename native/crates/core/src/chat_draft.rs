//! Chat draft destinations and directory preparation, without I/O.

use crate::chat_roots::ChatRoots;
use crate::path;

pub const CHAT_PROJECT_ID: &str = "openchamber:chats";
pub const CHAT_DRAFT_ID: &str = "chat-new-session";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftDestination {
    Chat,
    Project(String),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OpenRequest<'a> {
    pub chat: bool,
    pub directory: Option<&'a str>,
    pub last: Option<&'a DraftDestination>,
}

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
            Some(directory) if !roots.is_some_and(|roots| roots.contains(Some(&directory))) => {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootsStatus {
    Pending,
    Ready,
    Missing,
    Failed,
}

impl RootsStatus {
    pub fn of(roots: Option<&ChatRoots>, answered: bool, failed: bool) -> Self {
        match (roots, answered, failed) {
            (Some(_), _, _) => Self::Ready,
            (None, true, _) => Self::Missing,
            (None, false, true) => Self::Failed,
            _ => Self::Pending,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChatDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

pub fn session_directory(roots: &ChatRoots, date: ChatDate, id: &str) -> Option<String> {
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
        || !(1..=12).contains(&date.month)
        || !(1..=31).contains(&date.day)
    {
        return None;
    }
    Some(format!(
        "{}/{:04}-{:02}-{:02}/session-{id}",
        roots.canonical_configured().trim_end_matches('/'),
        date.year,
        date.month,
        date.day
    ))
}

/// A named directory gains execution authority only after mkdir succeeds.
/// A handoff retains custody through an unknown create outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Preparation {
    #[default]
    Named,
    Preparing,
    Ready,
    Failed,
    HandedOff,
}

impl Preparation {
    pub fn begin(&mut self) -> bool {
        if !matches!(self, Self::Named | Self::Failed) {
            return false;
        }
        *self = Self::Preparing;
        true
    }

    pub fn complete(&mut self, succeeded: bool) {
        if *self == Self::Preparing {
            *self = if succeeded { Self::Ready } else { Self::Failed };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> ChatRoots {
        ChatRoots::from_home_info(
            Some("/home/me"),
            Some("/data/chats"),
            Some("/real/chats"),
            Some("/real/legacy"),
        )
        .expect("chat roots")
    }

    #[test]
    fn shared_roots_and_traversal_never_grant_cleanup_authority() {
        let roots = roots();
        for path in [
            "/data/chats",
            "/data/chats/",
            "/real/chats",
            "/real/legacy",
            "/home/me/.config/openchamber/chats",
            "/data/chats/../other",
            "/real/chats/./session",
            "/data/chats//session",
            "/elsewhere/session",
        ] {
            assert_eq!(roots.session_suffix(path), None, "{path}");
        }
        assert!(roots.same_session_directory(
            "/data/chats/2026-11-13/session-a",
            "/real/chats/2026-11-13/session-a"
        ));
        assert!(!roots.same_session_directory("/data/chats/a", "/real/chats/b"));
        assert!(!roots.same_session_directory("/data/chats/a", "/real/legacy/a"));
    }

    #[test]
    fn entry_points_keep_explicit_directories_and_default_to_chat() {
        let roots = roots();
        assert_eq!(
            resolve_destination(OpenRequest::default(), Some(&roots), |_| false),
            DraftDestination::Chat
        );
        let project = DraftDestination::Project("/work/app".into());
        assert_eq!(
            resolve_destination(
                OpenRequest {
                    last: Some(&project),
                    ..Default::default()
                },
                Some(&roots),
                |_| true
            ),
            project
        );
        assert_eq!(
            resolve_destination(
                OpenRequest {
                    directory: Some("/external"),
                    ..Default::default()
                },
                Some(&roots),
                |_| false
            ),
            DraftDestination::Project("/external".into())
        );
        assert_eq!(
            resolve_destination(
                OpenRequest {
                    directory: Some("/real/chats/session-a"),
                    ..Default::default()
                },
                Some(&roots),
                |_| true
            ),
            DraftDestination::Chat
        );
    }

    #[test]
    fn preparation_shares_pending_work_and_requires_explicit_retry() {
        let mut state = Preparation::Named;
        assert!(state.begin());
        assert!(!state.begin());
        state.complete(false);
        assert_eq!(state, Preparation::Failed);
        assert!(state.begin());
        state.complete(true);
        assert_eq!(state, Preparation::Ready);
        assert!(!state.begin());
        state = Preparation::HandedOff;
        state.complete(false);
        assert_eq!(state, Preparation::HandedOff);
    }

    #[test]
    fn dated_directory_and_root_discovery_keep_their_exact_states() {
        assert_eq!(
            session_directory(
                &roots(),
                ChatDate {
                    year: 2026,
                    month: 11,
                    day: 13
                },
                "a-b"
            )
            .as_deref(),
            Some("/real/chats/2026-11-13/session-a-b")
        );
        assert_eq!(
            session_directory(
                &roots(),
                ChatDate {
                    year: 2026,
                    month: 11,
                    day: 13
                },
                "../a"
            ),
            None
        );
        assert_eq!(RootsStatus::of(None, false, false), RootsStatus::Pending);
        assert_eq!(RootsStatus::of(None, false, true), RootsStatus::Failed);
        assert_eq!(RootsStatus::of(None, true, false), RootsStatus::Missing);
    }
}
