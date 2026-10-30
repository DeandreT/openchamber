//! Session row menu actions: which actions a row offers, which sessions an
//! action reaches, and the optimistic pin overlay.
//!
//! Ports the decisions of `SessionNodeItem.tsx` (`renderSessionMenuItems`),
//! `sessionSubtreeActions.ts`, `sessionLineage.ts`, `session-restore-rule.ts`
//! and `session-pins.ts`. Rename, archive, restore, delete and work changes
//! wait for the server; only pins flip at once, as the web client does.

use std::collections::{HashMap, HashSet};

use crate::ids::SessionId;
use crate::session::SessionRecord;
use crate::session_index::SessionIndex;

/// One action of a session row's menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionAction {
    Rename,
    Pin,
    Unpin,
    /// Track as in work.
    TrackWork,
    /// Mark the work done.
    MarkDone,
    Archive,
    Restore,
    Delete,
}

impl SessionAction {
    /// A stable name for accessibility identities and automation.
    pub fn id(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::Pin => "pin",
            Self::Unpin => "unpin",
            Self::TrackWork => "track",
            Self::MarkDone => "mark-done",
            Self::Archive => "archive",
            Self::Restore => "restore",
            Self::Delete => "delete",
        }
    }

    pub fn label_key(self) -> &'static str {
        match self {
            Self::Rename => "sessions.sidebar.session.menu.rename",
            Self::Pin => "sessions.sidebar.session.menu.pin",
            Self::Unpin => "sessions.sidebar.session.menu.unpin",
            Self::TrackWork => "sessions.sidebar.session.work.track",
            Self::MarkDone => "sessions.sidebar.session.work.markDone",
            Self::Archive => "sessions.sidebar.bulkActions.archive",
            Self::Restore => "sessions.sidebar.bulkActions.restore",
            Self::Delete => "sessions.sidebar.bulkActions.delete",
        }
    }

    /// The explanation shown after the pointer rests on the item.
    pub fn hint_key(self) -> &'static str {
        match self {
            Self::Rename => "sessions.sidebar.session.menuHint.rename",
            Self::Pin => "sessions.sidebar.session.menuHint.pin",
            Self::Unpin => "sessions.sidebar.session.menuHint.unpin",
            Self::TrackWork => "sessions.sidebar.session.menuHint.track",
            Self::MarkDone => "sessions.sidebar.session.menuHint.markDone",
            Self::Archive => "sessions.sidebar.session.menuHint.archive",
            Self::Restore => "sessions.sidebar.session.menuHint.restore",
            Self::Delete => "sessions.sidebar.session.menuHint.delete",
        }
    }

    /// The sprite glyph drawn before the label.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Rename => "pencil-ai",
            Self::Pin => "pushpin",
            Self::Unpin => "unpin",
            Self::TrackWork => "eye",
            Self::MarkDone => "check",
            Self::Archive => "inbox-archive",
            Self::Restore => "inbox-unarchive",
            Self::Delete => "delete-bin",
        }
    }

    pub fn is_destructive(self) -> bool {
        self == Self::Delete
    }

    /// Whether the action writes to the server.
    fn mutates(self) -> bool {
        self != Self::Rename
    }
}

/// What a row's menu depends on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MenuContext {
    pub pinned: bool,
    /// `canTrackWork`: the row is a work of its own that may be tracked.
    pub can_track_work: bool,
    pub in_work: bool,
    /// The row lists an archived session.
    pub archived: bool,
    /// A server can take the change; false for synthetic data or while
    /// disconnected.
    pub available: bool,
    /// An archive, restore or delete of this session is in flight.
    pub removing: bool,
    /// A rename of this session is in flight.
    pub renaming: bool,
    /// A work change of this session is in flight.
    pub changing_work: bool,
}

/// One line of a row's menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEntry {
    Item {
        action: SessionAction,
        enabled: bool,
    },
    Separator,
}

impl MenuEntry {
    pub fn action(self) -> Option<SessionAction> {
        match self {
            Self::Item { action, .. } => Some(action),
            Self::Separator => None,
        }
    }

    pub fn is_enabled(self) -> bool {
        matches!(self, Self::Item { enabled: true, .. })
    }
}

/// The ported subset of the row menu, in the reference's order: Rename, Pin
/// or Unpin, Track or Mark done, then Archive or Restore and Delete after a
/// separator.
pub fn menu_entries(context: MenuContext) -> Vec<MenuEntry> {
    let enabled = |action: SessionAction| {
        let busy = match action {
            SessionAction::Rename => context.renaming || context.removing,
            SessionAction::TrackWork | SessionAction::MarkDone => {
                context.changing_work || context.removing
            }
            SessionAction::Pin | SessionAction::Unpin => context.removing,
            SessionAction::Archive | SessionAction::Restore | SessionAction::Delete => {
                context.removing
            }
        };
        (context.available || !action.mutates()) && !busy
    };
    let item = |action| MenuEntry::Item {
        action,
        enabled: enabled(action),
    };
    let mut entries = vec![
        item(SessionAction::Rename),
        item(if context.pinned {
            SessionAction::Unpin
        } else {
            SessionAction::Pin
        }),
    ];
    if context.can_track_work {
        entries.push(item(if context.in_work {
            SessionAction::MarkDone
        } else {
            SessionAction::TrackWork
        }));
    }
    entries.push(MenuEntry::Separator);
    entries.push(item(if context.archived {
        SessionAction::Restore
    } else {
        SessionAction::Archive
    }));
    entries.push(item(SessionAction::Delete));
    entries
}

/// The enabled item `step` places from `from`, wrapping around; `None` when
/// the menu has no enabled item.
pub fn step_highlight(entries: &[MenuEntry], from: Option<usize>, step: isize) -> Option<usize> {
    let count = entries.len();
    if count == 0 {
        return None;
    }
    let start = match from {
        Some(index) => index as isize,
        None if step > 0 => -1,
        None => count as isize,
    };
    (1..=count as isize)
        .map(|offset| (start + offset * step.signum()).rem_euclid(count as isize) as usize)
        .find(|index| entries[*index].is_enabled())
}

/// The first or last enabled item.
pub fn edge_highlight(entries: &[MenuEntry], last: bool) -> Option<usize> {
    if last {
        entries.iter().rposition(|entry| entry.is_enabled())
    } else {
        entries.iter().position(|entry| entry.is_enabled())
    }
}

/// `canTrackWork`: only a root work row outside an archive and outside
/// managed Chats offers Track and Done.
pub fn can_track_work(
    work_enabled: bool,
    owns_work: bool,
    archived_row: bool,
    session: &SessionRecord,
    in_chats: bool,
) -> bool {
    work_enabled && owns_work && !archived_row && session.parent_id.is_none() && !in_chats
}

/// The title a rename commits: the trimmed draft, unless it is empty or the
/// session's current title.
pub fn rename_title(current: &str, draft: &str) -> Option<String> {
    let trimmed = draft.trim();
    (!trimmed.is_empty() && trimmed != current).then(|| trimmed.to_owned())
}

/// Which descendants an action reaches, as `sessionLineage.ts` defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lineage {
    /// Active sessions at any depth, also under an archived intermediate.
    ActiveThroughArchived,
    /// Every session at any depth.
    All,
    /// Archived sessions, stopping at an active one.
    Archived,
}

fn descendants(index: &SessionIndex, root: &SessionId, lineage: Lineage) -> Vec<SessionId> {
    let mut children: HashMap<&SessionId, Vec<&SessionRecord>> = HashMap::new();
    for session in index.active().iter().chain(index.archived()) {
        if let Some(parent) = &session.parent_id {
            children.entry(parent).or_default().push(session);
        }
    }
    let walks = |session: &SessionRecord| lineage != Lineage::Archived || session.is_archived();
    let keeps = |session: &SessionRecord| match lineage {
        Lineage::ActiveThroughArchived => !session.is_archived(),
        Lineage::All => true,
        Lineage::Archived => session.is_archived(),
    };
    let mut visited: HashSet<&SessionId> = HashSet::from([root]);
    let mut found = Vec::new();
    // Depth first, each parent before its children.
    let mut stack: Vec<&SessionRecord> = children
        .get(root)
        .map(|list| list.iter().rev().copied().collect())
        .unwrap_or_default();
    while let Some(session) = stack.pop() {
        if !visited.insert(&session.id) || !walks(session) {
            continue;
        }
        if keeps(session) {
            found.push(session.id.clone());
        }
        if let Some(list) = children.get(&session.id) {
            stack.extend(list.iter().rev().copied());
        }
    }
    found
}

/// Archive reaches the session and every active session below it, also
/// under an archived intermediate; it never restamps an archived one.
pub fn archive_targets(index: &SessionIndex, root: &SessionId) -> Vec<SessionId> {
    let mut ids = vec![root.clone()];
    ids.extend(descendants(index, root, Lineage::ActiveThroughArchived));
    ids
}

/// Delete reaches the whole subtree, archived sessions included.
pub fn delete_targets(index: &SessionIndex, root: &SessionId) -> Vec<SessionId> {
    let mut ids = vec![root.clone()];
    ids.extend(descendants(index, root, Lineage::All));
    ids
}

/// The archived subsessions a restored root brings back, parents first.
pub fn archived_subsessions(index: &SessionIndex, root: &SessionId) -> Vec<SessionId> {
    descendants(index, root, Lineage::Archived)
}

/// `isRestorableOnItsOwn`: a subsession comes back alone only while its
/// parent is active.
pub fn restorable_on_its_own(index: &SessionIndex, session: &SessionRecord) -> bool {
    match &session.parent_id {
        None => true,
        Some(parent) => index
            .get(parent)
            .is_some_and(|parent| !parent.is_archived()),
    }
}

/// Archived sessions the archive page lists, most recently archived first.
pub fn archive_page(index: &SessionIndex) -> Vec<&SessionRecord> {
    let mut sessions: Vec<&SessionRecord> = index
        .archived()
        .iter()
        .filter(|session| restorable_on_its_own(index, session))
        .collect();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.time.archived.unwrap_or(0)));
    sessions
}

/// Pin changes the row already shows while the server stores them.
///
/// Clicks on one session run in order, so the last one wins on the server
/// too; the overlay stays until the last of them settles, and the row then
/// shows the server's state again, which a failed write never changed.
#[derive(Debug, Default)]
pub struct PendingPins {
    pending: HashMap<SessionId, PendingPin>,
}

#[derive(Debug)]
struct PendingPin {
    pinned: bool,
    in_flight: usize,
}

impl PendingPins {
    /// Records a click asking for `pinned`. Returns whether a write is
    /// needed: a menu opened before the row changed can ask for the state
    /// the row already shows, which writes nothing.
    pub fn request(&mut self, session: &SessionRecord, pinned: bool) -> bool {
        if self.shows_pinned(session) == pinned {
            return false;
        }
        let entry = self
            .pending
            .entry(session.id.clone())
            .or_insert(PendingPin {
                pinned,
                in_flight: 0,
            });
        entry.pinned = pinned;
        entry.in_flight += 1;
        true
    }

    /// Records that one write for `session` finished, however it ended.
    /// Returns whether the overlay was released.
    pub fn settle(&mut self, session: &SessionId) -> bool {
        let Some(entry) = self.pending.get_mut(session) else {
            return false;
        };
        entry.in_flight = entry.in_flight.saturating_sub(1);
        if entry.in_flight > 0 {
            return false;
        }
        self.pending.remove(session);
        true
    }

    /// Whether the row shows the session pinned.
    pub fn shows_pinned(&self, session: &SessionRecord) -> bool {
        self.pending
            .get(&session.id)
            .map_or(session.overlay.pinned_at.is_some(), |pin| pin.pinned)
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// The records with the overlay applied, for a sidebar projection.
    /// `None` when nothing is pending, so the caller keeps its slice.
    pub fn overlay(&self, sessions: &[SessionRecord]) -> Option<Vec<SessionRecord>> {
        if self.pending.is_empty() {
            return None;
        }
        Some(
            sessions
                .iter()
                .map(|session| match self.pending.get(&session.id) {
                    Some(pin) if pin.pinned != session.overlay.pinned_at.is_some() => {
                        let mut session = session.clone();
                        // The reference's pending pin carries no time; the
                        // session sorts as pinned until the server answers.
                        session.overlay.pinned_at = pin.pinned.then_some(0);
                        session
                    }
                    _ => session.clone(),
                })
                .collect(),
        )
    }

    /// Forgets every pending pin, for a runtime switch.
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionPatch, SessionTime};
    use crate::session_index::SessionChange;

    fn record(id: &str, parent: Option<&str>, archived: Option<i64>) -> SessionRecord {
        SessionRecord {
            parent_id: parent.map(SessionId::from),
            time: SessionTime {
                created: 1,
                updated: 1,
                idle: None,
                archived,
            },
            ..SessionRecord::new(id, "/p")
        }
    }

    fn index(records: Vec<SessionRecord>) -> SessionIndex {
        let mut index = SessionIndex::new();
        let ticket = index.begin_load();
        let _ = index.complete_load(ticket, records);
        index
    }

    fn ids(list: &[SessionId]) -> Vec<&str> {
        list.iter().map(SessionId::as_str).collect()
    }

    fn actions(entries: &[MenuEntry]) -> Vec<Option<SessionAction>> {
        entries.iter().map(|entry| entry.action()).collect()
    }

    #[test]
    fn the_menu_follows_the_reference_order_and_state() {
        let context = MenuContext {
            available: true,
            can_track_work: true,
            ..MenuContext::default()
        };
        assert_eq!(
            actions(&menu_entries(context)),
            [
                Some(SessionAction::Rename),
                Some(SessionAction::Pin),
                Some(SessionAction::TrackWork),
                None,
                Some(SessionAction::Archive),
                Some(SessionAction::Delete),
            ]
        );
        let archived = MenuContext {
            available: true,
            pinned: true,
            archived: true,
            ..MenuContext::default()
        };
        assert_eq!(
            actions(&menu_entries(archived)),
            [
                Some(SessionAction::Rename),
                Some(SessionAction::Unpin),
                None,
                Some(SessionAction::Restore),
                Some(SessionAction::Delete),
            ],
            "an archived row restores and has no work action"
        );
        let in_work = MenuContext {
            in_work: true,
            ..context
        };
        assert_eq!(
            menu_entries(in_work)[2].action(),
            Some(SessionAction::MarkDone)
        );
    }

    #[test]
    fn unavailable_and_pending_rows_disable_only_what_cannot_run() {
        let offline = menu_entries(MenuContext::default());
        assert!(offline[0].is_enabled(), "renaming opens the editor offline");
        assert!(
            offline.iter().skip(1).all(|entry| !entry.is_enabled()),
            "no server, no mutation"
        );
        let removing = menu_entries(MenuContext {
            available: true,
            removing: true,
            ..MenuContext::default()
        });
        assert!(removing.iter().all(|entry| !entry.is_enabled()));
        let renaming = menu_entries(MenuContext {
            available: true,
            renaming: true,
            ..MenuContext::default()
        });
        assert!(!renaming[0].is_enabled());
        assert!(renaming[1].is_enabled(), "a rename leaves pinning alone");
    }

    #[test]
    fn highlight_skips_separators_and_disabled_items_and_wraps() {
        let entries = menu_entries(MenuContext {
            available: true,
            ..MenuContext::default()
        });
        // Rename, Pin, separator, Archive, Delete.
        assert_eq!(step_highlight(&entries, None, 1), Some(0));
        assert_eq!(step_highlight(&entries, Some(1), 1), Some(3));
        assert_eq!(step_highlight(&entries, Some(4), 1), Some(0));
        assert_eq!(step_highlight(&entries, None, -1), Some(4));
        assert_eq!(step_highlight(&entries, Some(0), -1), Some(4));
        assert_eq!(edge_highlight(&entries, true), Some(4));
        let offline = menu_entries(MenuContext::default());
        assert_eq!(step_highlight(&offline, Some(0), 1), Some(0));
    }

    #[test]
    fn a_rename_commits_only_a_new_nonempty_title() {
        assert_eq!(rename_title("Old", "  New  ").as_deref(), Some("New"));
        assert_eq!(rename_title("Old", "   "), None);
        assert_eq!(rename_title("Old", " Old "), None);
    }

    #[test]
    fn work_tracking_belongs_to_root_work_rows() {
        let root = record("a", None, None);
        let child = record("b", Some("a"), None);
        assert!(can_track_work(true, true, false, &root, false));
        assert!(!can_track_work(false, true, false, &root, false));
        assert!(!can_track_work(true, false, false, &root, false));
        assert!(!can_track_work(true, true, true, &root, false));
        assert!(!can_track_work(true, true, false, &child, false));
        assert!(!can_track_work(true, true, false, &root, true));
    }

    #[test]
    fn subtree_targets_follow_the_lineage_policies() {
        // a -> b (archived) -> c (active); a -> d (archived).
        let index = index(vec![
            record("a", None, None),
            record("b", Some("a"), Some(5)),
            record("c", Some("b"), None),
            record("d", Some("a"), Some(6)),
        ]);
        let root = SessionId::from("a");
        assert_eq!(ids(&archive_targets(&index, &root)), ["a", "c"]);
        let mut all = delete_targets(&index, &root);
        all.sort();
        assert_eq!(ids(&all), ["a", "b", "c", "d"]);
        let mut archived = archived_subsessions(&index, &root);
        archived.sort();
        assert_eq!(ids(&archived), ["b", "d"], "restore stops at an active one");
    }

    #[test]
    fn a_subsession_restores_alone_only_under_an_active_parent() {
        let index = index(vec![
            record("a", None, None),
            record("b", Some("a"), Some(5)),
            record("x", None, Some(9)),
            record("y", Some("x"), Some(9)),
        ]);
        let listed: Vec<&str> = archive_page(&index)
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        assert_eq!(listed, ["x", "b"], "newest archive first; y waits for x");
    }

    #[test]
    fn pins_flip_at_once_and_settle_back_to_the_server_state() {
        let mut index = index(vec![record("a", None, None), record("b", None, None)]);
        let mut pins = PendingPins::default();
        let a = index.get(&SessionId::from("a")).expect("listed").clone();
        assert!(pins.request(&a, true), "an unpinned row asks to pin");
        assert!(pins.shows_pinned(&a));
        let overlaid = pins.overlay(index.active()).expect("a pin is pending");
        assert!(
            overlaid
                .iter()
                .any(|s| s.id == a.id && s.overlay.pinned_at.is_some())
        );
        assert!(
            overlaid
                .iter()
                .any(|s| s.id.as_str() == "b" && s.overlay.pinned_at.is_none()),
            "other sessions keep their state"
        );
        // A second click before the first answer unpins again; last wins.
        assert!(pins.request(&a, false));
        assert!(!pins.settle(&a.id), "one write is still in flight");
        assert!(!pins.shows_pinned(&a));
        // The server stored the pin of the last write that succeeded.
        let _ = index.apply([SessionChange::Patched {
            session_id: a.id.clone(),
            patch: SessionPatch {
                metadata: Some(crate::session::MetadataFacts::default()),
                ..SessionPatch::default()
            },
        }]);
        assert!(pins.settle(&a.id));
        assert!(pins.is_empty());
        assert!(pins.overlay(index.active()).is_none());
    }

    #[test]
    fn a_stale_menu_never_writes_the_opposite_state() {
        let mut pins = PendingPins::default();
        let session = record("a", None, None);
        // The menu said Unpin, but the row already shows the session unpinned.
        assert!(!pins.request(&session, false), "nothing to write");
        assert!(pins.is_empty());
        assert!(pins.request(&session, true));
        // A second Pin from a menu opened before the first click.
        assert!(
            !pins.request(&session, true),
            "the row already shows it pinned"
        );
        assert!(pins.shows_pinned(&session));
        assert!(pins.settle(&session.id), "only one write was queued");
    }

    #[test]
    fn a_failed_pin_shows_the_unchanged_server_state_after_settling() {
        let mut pins = PendingPins::default();
        let session = record("a", None, None);
        assert!(pins.request(&session, true));
        assert!(pins.settle(&session.id));
        assert!(!pins.shows_pinned(&session), "nothing reached the record");
    }
}
