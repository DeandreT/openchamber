//! How a session answers tool permissions, as the server's
//! `permission-auto-accept` runtime keeps it, and which fresh requests the
//! client keeps out of sight while the server decides them.
//!
//! Ports `stores/utils/permissionAutoAccept.ts`, the snapshot ordering of
//! `stores/permissionStore.ts` and the hold-back bookkeeping of
//! `sync/sync-context.tsx`. The server is the only responder: nothing here
//! answers a request.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::ids::{PermissionId, SessionId};

/// A session's permission policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PermissionMode {
    /// Every request waits for the user.
    #[default]
    Ask,
    /// Accepted unless the safety net says the user should decide.
    Safety,
    /// Every request is accepted.
    Auto,
}

impl PermissionMode {
    /// The mode the user sees: without a classification provider a `Safety`
    /// session waits for the user exactly as an `Ask` one does.
    #[must_use]
    pub fn displayed(self, safety_available: bool) -> Self {
        if self == Self::Safety && !safety_available {
            Self::Ask
        } else {
            self
        }
    }

    /// The shield button's cycle: Ask, Safety, Auto, Ask, skipping Safety
    /// while it is unavailable.
    #[must_use]
    pub fn next(self, safety_available: bool) -> Self {
        match self.displayed(safety_available) {
            Self::Ask if safety_available => Self::Safety,
            Self::Ask | Self::Safety => Self::Auto,
            Self::Auto => Self::Ask,
        }
    }

    /// Whether the server answers this mode's requests without the user:
    /// `Auto` always, `Safety` while the safety net can run.
    #[must_use]
    pub fn answers_without_user(self, safety_available: bool) -> bool {
        match self {
            Self::Auto => true,
            Self::Safety => safety_available,
            Self::Ask => false,
        }
    }
}

/// Whether `directory` lies inside an isolated space, `/spaces/<id>` with a
/// 12-digit lowercase hex id, as `spaceIdOfDirectory` in
/// `lib/spaces/space-route.ts` decides. A space session's mode lives in that
/// space's own policy, never in the host's.
#[must_use]
pub fn is_space_directory(directory: &str) -> bool {
    let Some(rest) = directory.strip_prefix("/spaces/") else {
        return false;
    };
    let (Some(id), Some(tail)) = (rest.get(..12), rest.get(12..)) else {
        return false;
    };
    id.bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && (tail.is_empty() || tail.starts_with('/'))
}

/// A policy as the server sends it, its legacy on/off view already folded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicySnapshot {
    pub modes: BTreeMap<SessionId, PermissionMode>,
    /// Absent from servers that predate revisions.
    pub revision: Option<u64>,
}

/// The nearest explicit mode up `session`'s lineage, `Ask` when there is
/// none. `parent` names a session's parent; a cycle ends the walk.
pub fn resolve(
    modes: &BTreeMap<SessionId, PermissionMode>,
    session: &SessionId,
    parent: impl Fn(&SessionId) -> Option<SessionId>,
) -> PermissionMode {
    let mut seen = BTreeSet::new();
    let mut current = Some(session.clone());
    while let Some(id) = current {
        if let Some(mode) = modes.get(&id) {
            return *mode;
        }
        current = parent(&id);
        if !seen.insert(id) {
            break;
        }
    }
    PermissionMode::Ask
}

/// The client's projection of one runtime's policy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionPolicy {
    modes: BTreeMap<SessionId, PermissionMode>,
    /// The newest revision applied; `None` until a revisioned snapshot.
    revision: Option<u64>,
    loaded: bool,
}

impl PermissionPolicy {
    /// Applies a snapshot unless it is older than the one held. A snapshot
    /// without a revision only fills a policy that never held a revisioned
    /// one. Returns whether the policy changed hands.
    pub fn apply(&mut self, snapshot: PolicySnapshot) -> bool {
        match (snapshot.revision, self.revision) {
            (None, Some(_)) => return false,
            (Some(next), Some(held)) if next < held => return false,
            _ => {}
        }
        if snapshot.revision.is_some() {
            self.revision = snapshot.revision;
        }
        self.modes = snapshot.modes;
        self.loaded = true;
        true
    }

    /// Whether a snapshot was applied.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn revision(&self) -> Option<u64> {
        self.revision
    }

    /// The mode `session` runs in, from the nearest explicit ancestor.
    pub fn mode(
        &self,
        session: &SessionId,
        parent: impl Fn(&SessionId) -> Option<SessionId>,
    ) -> PermissionMode {
        resolve(&self.modes, session, parent)
    }
}

/// How many reports that arrived before their request are kept, as
/// `MAX_EARLY_LEFT_FOR_USER` in the web client.
pub const MAX_EARLY_LEFT_FOR_USER: usize = 100;

/// Fresh permission requests kept out of sight until the server reports it
/// left them for the user, keyed by session and request so a report names
/// only a request of its own session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoAnswerHolds {
    held: BTreeSet<(SessionId, PermissionId)>,
    /// Reports that arrived before their request, oldest first.
    early: VecDeque<(SessionId, PermissionId)>,
    /// Requests held while a reconnect's replay runs, from the stale
    /// connection until the reconnect's policy read lands.
    replay: Option<BTreeSet<(SessionId, PermissionId)>>,
}

/// What a `left-for-user` report did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeftForUser {
    /// A held request is shown now.
    Revealed,
    /// The request was already on screen.
    AlreadyShown,
    /// The request has not arrived; it is shown at once when it does.
    Remembered,
}

impl AutoAnswerHolds {
    /// Records a fresh `permission.asked`. Holds it when the server may
    /// answer it on its own and no report already left it for the user.
    /// Returns whether the request is held.
    pub fn asked(
        &mut self,
        session: &SessionId,
        permission: &PermissionId,
        answered_without_user: bool,
    ) -> bool {
        let key = (session.clone(), permission.clone());
        if let Some(index) = self.early.iter().position(|early| *early == key) {
            self.early.remove(index);
            return false;
        }
        if !answered_without_user {
            return false;
        }
        if let Some(replay) = &mut self.replay {
            replay.insert(key.clone());
        }
        self.held.insert(key);
        true
    }

    /// Records the server's report that it left a request for the user.
    /// `shown` says whether the request is already presented.
    pub fn left_for_user(
        &mut self,
        session: &SessionId,
        permission: &PermissionId,
        shown: bool,
    ) -> LeftForUser {
        let key = (session.clone(), permission.clone());
        if self.held.remove(&key) {
            return LeftForUser::Revealed;
        }
        if shown {
            return LeftForUser::AlreadyShown;
        }
        if !self.early.contains(&key) {
            self.early.push_back(key);
            if self.early.len() > MAX_EARLY_LEFT_FOR_USER {
                self.early.pop_front();
            }
        }
        LeftForUser::Remembered
    }

    /// Forgets a request that was answered.
    pub fn settled(&mut self, session: &SessionId, permission: &PermissionId) {
        let key = (session.clone(), permission.clone());
        self.held.remove(&key);
        self.early.retain(|early| *early != key);
    }

    pub fn is_held(&self, session: &SessionId, permission: &PermissionId) -> bool {
        self.held.contains(&(session.clone(), permission.clone()))
    }

    /// Shows every held request: a gap in the event stream may have lost
    /// the report that would have revealed it.
    pub fn release_all(&mut self) {
        self.held.clear();
    }

    /// Starts a reconnect after a gap: shows every held request and marks
    /// the requests held from now on as replayed. The server sends `ready`
    /// before its replay, and `left-for-user` is never replayed, so a
    /// request first asked during the gap would otherwise stay hidden.
    pub fn open_replay_window(&mut self) {
        self.release_all();
        self.replay = Some(BTreeSet::new());
    }

    /// Ends the replay window, showing every request held inside it.
    /// Returns how many were shown; none when no window was open.
    pub fn close_replay_window(&mut self) -> usize {
        let Some(replay) = self.replay.take() else {
            return 0;
        };
        let before = self.held.len();
        self.held.retain(|key| !replay.contains(key));
        before - self.held.len()
    }

    /// Shows the held requests of every session `release` picks, such as
    /// one the server no longer answers without the user: it then answers
    /// `ignored` and never reports the request left for the user. Returns
    /// how many were shown.
    pub fn release_sessions(&mut self, mut release: impl FnMut(&SessionId) -> bool) -> usize {
        let before = self.held.len();
        self.held.retain(|(session, _)| !release(session));
        before - self.held.len()
    }

    pub fn early_len(&self) -> usize {
        self.early.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> SessionId {
        SessionId::from(value)
    }

    #[test]
    fn space_directories_match_the_web_route() {
        assert!(is_space_directory("/spaces/0123456789ab"));
        assert!(is_space_directory("/spaces/0123456789ab/my repo"));
        for directory in [
            "/home/spaces/0123456789ab",
            "/spaces/0123456789AB/repo",
            "/spaces/0123456789a/repo",
            "/spaces/0123456789abc",
            "/spaces/0123456789aé/repo",
            "/spaces/",
            "",
        ] {
            assert!(!is_space_directory(directory), "{directory}");
        }
    }

    #[test]
    fn requests_held_in_a_replay_window_show_when_it_closes() {
        let session = id("s");
        let (before, replayed, after) = (
            PermissionId::from("per_before"),
            PermissionId::from("per_replayed"),
            PermissionId::from("per_after"),
        );
        let mut holds = AutoAnswerHolds::default();
        assert_eq!(holds.close_replay_window(), 0, "no window is open");
        assert!(holds.asked(&session, &before, true));
        holds.open_replay_window();
        assert!(!holds.is_held(&session, &before), "the gap shows old holds");
        assert!(holds.asked(&session, &replayed, true));
        assert!(holds.is_held(&session, &replayed));
        assert_eq!(holds.close_replay_window(), 1);
        assert!(!holds.is_held(&session, &replayed));
        assert!(holds.asked(&session, &after, true));
        assert_eq!(holds.close_replay_window(), 0);
        assert!(
            holds.is_held(&session, &after),
            "live holds outlast the window"
        );
    }

    #[test]
    fn holds_release_by_session() {
        let mut holds = AutoAnswerHolds::default();
        let (asked, kept) = (id("asked"), id("kept"));
        assert!(holds.asked(&asked, &PermissionId::from("per_1"), true));
        assert!(holds.asked(&asked, &PermissionId::from("per_2"), true));
        assert!(holds.asked(&kept, &PermissionId::from("per_3"), true));
        assert_eq!(holds.release_sessions(|session| *session == asked), 2);
        assert!(!holds.is_held(&asked, &PermissionId::from("per_1")));
        assert!(holds.is_held(&kept, &PermissionId::from("per_3")));
        assert_eq!(holds.release_sessions(|_| false), 0);
    }

    #[test]
    fn the_cycle_skips_safety_while_it_is_unavailable() {
        use PermissionMode::{Ask, Auto, Safety};
        assert_eq!(Ask.next(true), Safety);
        assert_eq!(Safety.next(true), Auto);
        assert_eq!(Auto.next(true), Ask);
        assert_eq!(Ask.next(false), Auto);
        assert_eq!(Auto.next(false), Ask);
        // A stored Safety shows and cycles as Ask while unavailable.
        assert_eq!(Safety.displayed(false), Ask);
        assert_eq!(Safety.next(false), Auto);
        assert_eq!(Safety.displayed(true), Safety);
    }

    #[test]
    fn only_auto_and_an_available_safety_net_answer_without_the_user() {
        use PermissionMode::{Ask, Auto, Safety};
        assert!(Auto.answers_without_user(false));
        assert!(Safety.answers_without_user(true));
        assert!(!Safety.answers_without_user(false));
        assert!(!Ask.answers_without_user(true));
    }

    #[test]
    fn the_nearest_explicit_ancestor_wins_and_cycles_fall_back_to_ask() {
        let parents: BTreeMap<SessionId, SessionId> = [
            (id("child"), id("parent")),
            (id("parent"), id("root")),
            (id("loop-a"), id("loop-b")),
            (id("loop-b"), id("loop-a")),
        ]
        .into();
        let parent = |session: &SessionId| parents.get(session).cloned();
        let mut modes = BTreeMap::new();
        modes.insert(id("root"), PermissionMode::Auto);
        assert_eq!(resolve(&modes, &id("child"), parent), PermissionMode::Auto);
        modes.insert(id("parent"), PermissionMode::Ask);
        assert_eq!(
            resolve(&modes, &id("child"), parent),
            PermissionMode::Ask,
            "a nearer explicit ask overrides an auto root"
        );
        assert_eq!(resolve(&modes, &id("loop-a"), parent), PermissionMode::Ask);
        assert_eq!(resolve(&modes, &id("unknown"), parent), PermissionMode::Ask);
    }

    #[test]
    fn older_revisions_and_unrevisioned_snapshots_never_replace_newer_ones() {
        let snapshot = |mode, revision| PolicySnapshot {
            modes: [(id("s"), mode)].into(),
            revision,
        };
        let mut policy = PermissionPolicy::default();
        assert!(!policy.is_loaded());
        assert!(policy.apply(snapshot(PermissionMode::Ask, None)));
        assert!(policy.is_loaded());
        assert!(policy.apply(snapshot(PermissionMode::Auto, Some(3))));
        assert!(!policy.apply(snapshot(PermissionMode::Ask, Some(2))));
        assert!(!policy.apply(snapshot(PermissionMode::Ask, None)));
        assert_eq!(policy.mode(&id("s"), |_| None), PermissionMode::Auto);
        assert!(
            policy.apply(snapshot(PermissionMode::Safety, Some(3))),
            "the same revision applies again, as a broadcast echo does"
        );
        assert_eq!(policy.revision(), Some(3));
    }

    #[test]
    fn held_requests_appear_on_their_report_and_early_reports_are_bounded() {
        let session = id("s");
        let first = PermissionId::from("per_1");
        let mut holds = AutoAnswerHolds::default();
        assert!(
            !holds.asked(&session, &first, false),
            "ask sessions show at once"
        );
        assert!(holds.asked(&session, &first, true));
        assert!(holds.is_held(&session, &first));
        assert_eq!(
            holds.left_for_user(&id("other"), &first, false),
            LeftForUser::Remembered,
            "a report names its own session's request only"
        );
        assert!(holds.is_held(&session, &first));
        assert_eq!(
            holds.left_for_user(&session, &first, false),
            LeftForUser::Revealed
        );
        assert!(!holds.is_held(&session, &first));

        let early = PermissionId::from("per_2");
        assert_eq!(
            holds.left_for_user(&session, &early, false),
            LeftForUser::Remembered
        );
        assert!(
            !holds.asked(&session, &early, true),
            "an early report shows it"
        );
        assert!(
            holds.asked(&session, &early, true),
            "the report is used once"
        );
        holds.settled(&session, &early);
        assert!(!holds.is_held(&session, &early));

        for index in 0..MAX_EARLY_LEFT_FOR_USER + 5 {
            let _ =
                holds.left_for_user(&session, &PermissionId::new(format!("per_{index}")), false);
        }
        assert_eq!(holds.early_len(), MAX_EARLY_LEFT_FOR_USER);
        assert_eq!(
            holds.left_for_user(&session, &PermissionId::from("x"), true),
            LeftForUser::AlreadyShown
        );
    }
}
