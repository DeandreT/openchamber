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
    /// Names the session from its latest completed turns on the small model.
    AiRename,
    CopyId,
    /// Opens the Pin to submenu.
    PinTo,
    /// Pin to submenu: the default Pinned list.
    Pin,
    /// Pin to submenu: leave the pin list.
    Unpin,
    /// Track as in work.
    TrackWork,
    /// Mark the work done.
    MarkDone,
    KeepWarm,
    StopWarm,
    ExportMarkdown,
    /// Opens the Move to worktree submenu.
    MoveToWorktree,
    /// Opens the dialog that moves the conversation to another project.
    MoveSession,
    OpenRunOverview,
    /// Opens the Move to folder submenu.
    MoveToFolder,
    OpenInSidePanel,
    OpenMiniChat,
    Archive,
    Restore,
    Delete,
}

impl SessionAction {
    /// A stable name for accessibility identities and automation.
    pub fn id(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::AiRename => "ai-rename",
            Self::CopyId => "copy-id",
            Self::PinTo => "pin-to",
            Self::Pin => "pin",
            Self::Unpin => "unpin",
            Self::TrackWork => "track",
            Self::MarkDone => "mark-done",
            Self::KeepWarm => "keep-warm",
            Self::StopWarm => "stop-warm",
            Self::ExportMarkdown => "export-markdown",
            Self::MoveToWorktree => "move-to-worktree",
            Self::MoveSession => "move-session",
            Self::OpenRunOverview => "run-overview",
            Self::MoveToFolder => "move-to-folder",
            Self::OpenInSidePanel => "side-panel",
            Self::OpenMiniChat => "mini-chat",
            Self::Archive => "archive",
            Self::Restore => "restore",
            Self::Delete => "delete",
        }
    }

    pub fn label_key(self) -> &'static str {
        match self {
            Self::Rename => "sessions.sidebar.session.menu.rename",
            Self::AiRename => "sessions.aiRename.action",
            Self::CopyId => "sessions.sidebar.session.menu.copyId",
            Self::PinTo => "sessions.sidebar.pinTo.label",
            Self::Pin => "sessions.sidebar.activity.pinnedTitle",
            Self::Unpin => "sessions.sidebar.session.menu.unpin",
            Self::TrackWork => "sessions.sidebar.session.work.track",
            Self::MarkDone => "sessions.sidebar.session.work.markDone",
            Self::KeepWarm => "sessions.sidebar.session.menu.keepWarm",
            Self::StopWarm => "sessions.sidebar.session.menu.stopWarm",
            Self::ExportMarkdown => "sessions.sidebar.session.menu.exportMarkdown",
            Self::MoveToWorktree => "sessions.sidebar.session.menu.moveToWorktreeTargets",
            Self::MoveSession => "sessions.moveSession.menu",
            Self::OpenRunOverview => "sessions.sidebar.session.menu.openRunOverview",
            Self::MoveToFolder => "sessions.sidebar.folders.moveToFolder",
            Self::OpenInSidePanel => "sessions.sidebar.session.menu.openInSidePanel",
            Self::OpenMiniChat => "sessions.sidebar.session.menu.openMiniChatWindow",
            Self::Archive => "sessions.sidebar.bulkActions.archive",
            Self::Restore => "sessions.sidebar.bulkActions.restore",
            Self::Delete => "sessions.sidebar.bulkActions.delete",
        }
    }

    /// The explanation shown after the pointer rests on the item, as the
    /// reference's `SessionMenuItemHint` wraps it. Items the reference
    /// leaves unwrapped have none; Move to worktree explains itself only
    /// while it is disabled.
    pub fn hint_key(self, context: &MenuContext) -> Option<&'static str> {
        Some(match self {
            Self::Rename => "sessions.sidebar.session.menuHint.rename",
            Self::AiRename => context.ai_rename.hint_key(),
            Self::CopyId => "sessions.sidebar.session.menuHint.copyId",
            Self::PinTo | Self::Pin | Self::Unpin => return None,
            Self::TrackWork => "sessions.sidebar.session.menuHint.track",
            Self::MarkDone => "sessions.sidebar.session.menuHint.markDone",
            Self::KeepWarm => "sessions.sidebar.session.menuHint.keepWarm",
            Self::StopWarm => "sessions.sidebar.session.menuHint.stopWarm",
            Self::ExportMarkdown => "sessions.sidebar.session.menuHint.exportMarkdown",
            Self::MoveToWorktree if context.moving => {
                "sessions.sidebar.session.moveToWorktree.tooltipMoving"
            }
            Self::MoveToWorktree if context.busy => {
                "sessions.sidebar.session.moveToWorktree.tooltipBusy"
            }
            Self::MoveToWorktree => return None,
            Self::MoveSession if context.busy => {
                "sessions.sidebar.session.moveToWorktree.tooltipBusy"
            }
            Self::MoveSession => "sessions.moveSession.hint",
            Self::OpenRunOverview => "sessions.sidebar.session.menuHint.openRunOverview",
            Self::MoveToFolder => "sessions.sidebar.session.menuHint.moveToFolder",
            Self::OpenInSidePanel => "sessions.sidebar.session.menuHint.openInSidePanel",
            Self::OpenMiniChat => "sessions.sidebar.session.menuHint.openMiniChatWindow",
            Self::Archive => "sessions.sidebar.session.menuHint.archive",
            Self::Restore => "sessions.sidebar.session.menuHint.restore",
            Self::Delete => "sessions.sidebar.session.menuHint.delete",
        })
    }

    /// The sprite glyph drawn before the label.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Rename => "pencil-ai",
            Self::AiRename => "ai-generate-2",
            Self::CopyId => "file-copy",
            Self::PinTo | Self::Pin => "pushpin",
            Self::Unpin => "unpin",
            Self::TrackWork => "eye",
            Self::MarkDone => "check",
            Self::KeepWarm => "fire",
            Self::StopWarm => "fire-fill",
            Self::ExportMarkdown => "download",
            Self::MoveToWorktree => "folder-shared",
            Self::MoveSession => "folder-received",
            Self::OpenRunOverview => "git-merge",
            Self::MoveToFolder => "folder",
            Self::OpenInSidePanel => "chat-4",
            Self::OpenMiniChat => "window",
            Self::Archive => "inbox-archive",
            Self::Restore => "inbox-unarchive",
            Self::Delete => "delete-bin",
        }
    }

    pub fn is_destructive(self) -> bool {
        self == Self::Delete
    }

    /// The submenu the item opens instead of running.
    pub fn submenu(self) -> Option<Submenu> {
        match self {
            Self::PinTo => Some(Submenu::Pin),
            Self::MoveToWorktree => Some(Submenu::Worktree),
            Self::MoveToFolder => Some(Submenu::Folder),
            _ => None,
        }
    }

    /// Whether the action needs a server. Rename opens its editor, Copy ID
    /// writes the clipboard, Pin to opens its submenu and folders queue
    /// their changes until the server is back, all without one.
    fn needs_server(self) -> bool {
        !matches!(
            self,
            Self::Rename | Self::CopyId | Self::PinTo | Self::MoveToFolder
        )
    }
}

/// A submenu of the row menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Submenu {
    Pin,
    Worktree,
    Folder,
}

/// Whether the AI rename can run, as `useSessionAiRenameAction` decides it
/// while the menu is open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AiRenameState {
    /// The history check has not answered yet.
    #[default]
    Checking,
    Ready,
    /// The session has no completed turn to name it from.
    Empty,
    /// The check failed; the item stays enabled and the run reports.
    Unknown,
    NoSmallModel,
    /// A rename of this session is generating.
    Pending,
}

impl AiRenameState {
    fn disables(self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Empty | Self::NoSmallModel | Self::Pending
        )
    }

    fn hint_key(self) -> &'static str {
        match self {
            Self::Pending => "sessions.aiRename.generating",
            Self::NoSmallModel => "sessions.aiRename.noSmallModel",
            Self::Checking => "sessions.aiRename.checking",
            Self::Empty => "sessions.aiRename.noCompletedTurns",
            Self::Ready | Self::Unknown => "sessions.sidebar.session.menuHint.aiRename",
        }
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
    /// The session's directory is known.
    pub has_directory: bool,
    /// The session has a parent.
    pub subtask: bool,
    /// The session lives in an isolated space.
    pub in_space: bool,
    /// The row is drawn in a zone or the list by time.
    pub timeline_row: bool,
    /// A turn of the session is running.
    pub busy: bool,
    /// A move of the session to a worktree or project is in flight.
    pub moving: bool,
    /// Settings chose per-session warming.
    pub warming_enabled: bool,
    pub warmed: bool,
    /// A warm change of this session is in flight.
    pub changing_warm: bool,
    /// The session belongs to a multi-run.
    pub multi_run: bool,
    /// Folder changes can be made for the session's scope.
    pub folder_authority: bool,
    pub ai_rename: AiRenameState,
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

/// The row menu in the reference's order (`renderSessionMenuItems`):
/// Rename, AI rename, Copy ID, Pin to, Track or Mark done, Keep or Stop
/// warm, Export Markdown, Move to worktree, Move session, Open run
/// overview, then Move to folder after a separator, Open in side panel,
/// Open Mini Chat window, and Archive or Restore with Delete after another.
pub fn menu_entries(context: MenuContext) -> Vec<MenuEntry> {
    let enabled = |action: SessionAction| {
        let blocked = match action {
            SessionAction::Rename => context.renaming,
            SessionAction::AiRename => !context.has_directory || context.ai_rename.disables(),
            SessionAction::TrackWork | SessionAction::MarkDone => context.changing_work,
            SessionAction::KeepWarm | SessionAction::StopWarm => context.changing_warm,
            SessionAction::MoveToWorktree => {
                !context.has_directory || context.busy || context.moving
            }
            SessionAction::MoveSession => context.busy || context.moving,
            SessionAction::OpenInSidePanel | SessionAction::OpenMiniChat => !context.has_directory,
            _ => false,
        };
        let removing = context.removing && action != SessionAction::CopyId;
        (context.available || !action.needs_server()) && !blocked && !removing
    };
    let item = |action| MenuEntry::Item {
        action,
        enabled: enabled(action),
    };
    let mut entries = vec![
        item(SessionAction::Rename),
        item(SessionAction::AiRename),
        item(SessionAction::CopyId),
        item(SessionAction::PinTo),
    ];
    if context.can_track_work {
        entries.push(item(if context.in_work {
            SessionAction::MarkDone
        } else {
            SessionAction::TrackWork
        }));
    }
    let can_keep_warm = context.warming_enabled
        && !context.archived
        && !context.subtask
        && context.has_directory
        && !context.in_space;
    if can_keep_warm {
        entries.push(item(if context.warmed {
            SessionAction::StopWarm
        } else {
            SessionAction::KeepWarm
        }));
    }
    entries.push(item(SessionAction::ExportMarkdown));
    if !context.subtask && !context.archived && context.has_directory {
        entries.push(item(SessionAction::MoveToWorktree));
    }
    if !context.subtask && !context.archived && context.has_directory && !context.in_space {
        entries.push(item(SessionAction::MoveSession));
    }
    if context.multi_run {
        entries.push(item(SessionAction::OpenRunOverview));
    }
    if context.has_directory
        && !context.archived
        && !context.timeline_row
        && context.folder_authority
    {
        entries.push(MenuEntry::Separator);
        entries.push(item(SessionAction::MoveToFolder));
    }
    entries.push(item(SessionAction::OpenInSidePanel));
    entries.push(item(SessionAction::OpenMiniChat));
    entries.push(MenuEntry::Separator);
    entries.push(item(if context.archived {
        SessionAction::Restore
    } else {
        SessionAction::Archive
    }));
    entries.push(item(SessionAction::Delete));
    entries
}

/// What a submenu item does.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SubmenuAction {
    /// Pin to the default Pinned list.
    Pin,
    Unpin,
    /// Move the session tree to an existing worktree.
    MoveToWorktree {
        path: String,
    },
    /// Create a worktree from the session's branch and move the tree there.
    NewWorktree,
    /// File the session in a folder of `scope`.
    AssignFolder {
        scope: String,
        folder_id: String,
    },
    /// Take the session out of its folder in `scope`.
    RemoveFromFolder {
        scope: String,
    },
    /// Create a folder in `scope` holding the session and rename it.
    NewFolder {
        scope: String,
    },
}

impl SubmenuAction {
    /// A stable name for accessibility identities and automation.
    pub fn id(&self) -> String {
        match self {
            Self::Pin => "pin".to_owned(),
            Self::Unpin => "unpin".to_owned(),
            Self::MoveToWorktree { path } => format!("worktree:{path}"),
            Self::NewWorktree => "new-worktree".to_owned(),
            Self::AssignFolder { folder_id, .. } => format!("folder:{folder_id}"),
            Self::RemoveFromFolder { .. } => "remove-from-folder".to_owned(),
            Self::NewFolder { .. } => "new-folder".to_owned(),
        }
    }
}

/// The text of a submenu line: a dictionary key or the user's own text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmenuLabel {
    Key(&'static str),
    Text(String),
}

/// One item of a submenu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmenuItem {
    pub action: SubmenuAction,
    pub label: SubmenuLabel,
    pub icon: Option<&'static str>,
    pub enabled: bool,
    /// A check marks the current choice.
    pub checked: bool,
    pub destructive: bool,
    /// The full path, shown when the pointer rests on a worktree.
    pub title: Option<String>,
}

impl SubmenuItem {
    pub fn new(action: SubmenuAction, label: SubmenuLabel) -> Self {
        Self {
            action,
            label,
            icon: None,
            enabled: true,
            checked: false,
            destructive: false,
            title: None,
        }
    }
}

/// One line of a submenu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmenuEntry {
    Item(SubmenuItem),
    /// A disabled line that says why nothing is listed.
    Note(&'static str),
    Separator,
}

impl SubmenuEntry {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Item(item) if item.enabled)
    }

    pub fn action(&self) -> Option<&SubmenuAction> {
        match self {
            Self::Item(item) => Some(&item.action),
            _ => None,
        }
    }
}

/// `PinToMenu`: Pinned with a check while pinned, and Unpin while pinned.
/// Named pin lists and New list belong to the custom sidebar sections,
/// which the native sidebar does not draw.
/// Without a server the pins cannot change, and the items say so.
pub fn pin_submenu(pinned: bool, available: bool) -> Vec<SubmenuEntry> {
    let mut entries = vec![SubmenuEntry::Item(SubmenuItem {
        checked: pinned,
        enabled: available,
        ..SubmenuItem::new(
            SubmenuAction::Pin,
            SubmenuLabel::Key("sessions.sidebar.activity.pinnedTitle"),
        )
    })];
    if pinned {
        entries.push(SubmenuEntry::Item(SubmenuItem {
            icon: Some("unpin"),
            enabled: available,
            ..SubmenuItem::new(
                SubmenuAction::Unpin,
                SubmenuLabel::Key("sessions.sidebar.session.menu.unpin"),
            )
        }));
    }
    entries
}

/// The enabled item `step` places from `from`, wrapping around; `None` when
/// the menu has no enabled item.
pub fn step_highlight_by(
    count: usize,
    enabled: impl Fn(usize) -> bool,
    from: Option<usize>,
    step: isize,
) -> Option<usize> {
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
        .find(|index| enabled(*index))
}

/// The enabled item `step` places from `from` in the row menu.
pub fn step_highlight(entries: &[MenuEntry], from: Option<usize>, step: isize) -> Option<usize> {
    step_highlight_by(
        entries.len(),
        |index| entries[index].is_enabled(),
        from,
        step,
    )
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

    fn ready() -> MenuContext {
        MenuContext {
            available: true,
            has_directory: true,
            folder_authority: true,
            ai_rename: AiRenameState::Ready,
            ..MenuContext::default()
        }
    }

    #[test]
    fn the_menu_follows_the_reference_order_and_state() {
        use SessionAction::*;
        let context = MenuContext {
            can_track_work: true,
            warming_enabled: true,
            multi_run: true,
            ..ready()
        };
        assert_eq!(
            actions(&menu_entries(context)),
            [
                Some(Rename),
                Some(AiRename),
                Some(CopyId),
                Some(PinTo),
                Some(TrackWork),
                Some(KeepWarm),
                Some(ExportMarkdown),
                Some(MoveToWorktree),
                Some(MoveSession),
                Some(OpenRunOverview),
                None,
                Some(MoveToFolder),
                Some(OpenInSidePanel),
                Some(OpenMiniChat),
                None,
                Some(Archive),
                Some(Delete),
            ]
        );
        let archived = MenuContext {
            archived: true,
            warming_enabled: true,
            ..ready()
        };
        assert_eq!(
            actions(&menu_entries(archived)),
            [
                Some(Rename),
                Some(AiRename),
                Some(CopyId),
                Some(PinTo),
                Some(ExportMarkdown),
                Some(OpenInSidePanel),
                Some(OpenMiniChat),
                None,
                Some(Restore),
                Some(Delete),
            ],
            "an archived row restores and has no work, warm, move or folder action"
        );
        let in_work = MenuContext {
            in_work: true,
            can_track_work: true,
            warmed: true,
            warming_enabled: true,
            ..ready()
        };
        let entries = actions(&menu_entries(in_work));
        assert_eq!(entries[4], Some(MarkDone));
        assert_eq!(entries[5], Some(StopWarm));
    }

    #[test]
    fn subtasks_spaces_and_timeline_rows_drop_what_the_reference_drops() {
        use SessionAction::*;
        let subtask = actions(&menu_entries(MenuContext {
            subtask: true,
            warming_enabled: true,
            ..ready()
        }));
        assert!(!subtask.contains(&Some(KeepWarm)));
        assert!(!subtask.contains(&Some(MoveToWorktree)));
        assert!(!subtask.contains(&Some(MoveSession)));
        let space = actions(&menu_entries(MenuContext {
            in_space: true,
            warming_enabled: true,
            ..ready()
        }));
        assert!(!space.contains(&Some(KeepWarm)));
        assert!(space.contains(&Some(MoveToWorktree)));
        assert!(!space.contains(&Some(MoveSession)));
        let timeline = actions(&menu_entries(MenuContext {
            timeline_row: true,
            ..ready()
        }));
        assert!(!timeline.contains(&Some(MoveToFolder)));
        let nowhere = menu_entries(MenuContext {
            has_directory: false,
            ..ready()
        });
        assert!(!actions(&nowhere).contains(&Some(MoveToWorktree)));
        let side_panel = nowhere
            .iter()
            .find(|entry| entry.action() == Some(OpenInSidePanel))
            .expect("always listed");
        assert!(!side_panel.is_enabled(), "it needs the session's directory");
    }

    #[test]
    fn unavailable_and_pending_rows_disable_only_what_cannot_run() {
        use SessionAction::*;
        let offline = menu_entries(MenuContext {
            available: false,
            ..ready()
        });
        let enabled: Vec<_> = offline
            .iter()
            .filter(|entry| entry.is_enabled())
            .filter_map(|entry| entry.action())
            .collect();
        assert_eq!(
            enabled,
            [Rename, CopyId, PinTo, MoveToFolder],
            "no server, no other action"
        );
        let removing = menu_entries(MenuContext {
            removing: true,
            ..ready()
        });
        let enabled: Vec<_> = removing
            .iter()
            .filter(|entry| entry.is_enabled())
            .filter_map(|entry| entry.action())
            .collect();
        assert_eq!(enabled, [CopyId]);
        let renaming = menu_entries(MenuContext {
            renaming: true,
            ..ready()
        });
        assert!(!renaming[0].is_enabled());
        assert!(renaming[3].is_enabled(), "a rename leaves pinning alone");
        let busy = menu_entries(MenuContext {
            busy: true,
            ..ready()
        });
        for action in [MoveToWorktree, MoveSession] {
            let entry = busy
                .iter()
                .find(|entry| entry.action() == Some(action))
                .expect("listed");
            assert!(!entry.is_enabled(), "{action:?} waits for the turn");
        }
    }

    #[test]
    fn ai_rename_explains_why_it_is_disabled() {
        for (state, enabled, hint) in [
            (AiRenameState::Checking, false, "sessions.aiRename.checking"),
            (
                AiRenameState::Empty,
                false,
                "sessions.aiRename.noCompletedTurns",
            ),
            (
                AiRenameState::NoSmallModel,
                false,
                "sessions.aiRename.noSmallModel",
            ),
            (
                AiRenameState::Pending,
                false,
                "sessions.aiRename.generating",
            ),
            (
                AiRenameState::Unknown,
                true,
                "sessions.sidebar.session.menuHint.aiRename",
            ),
            (
                AiRenameState::Ready,
                true,
                "sessions.sidebar.session.menuHint.aiRename",
            ),
        ] {
            let context = MenuContext {
                ai_rename: state,
                ..ready()
            };
            assert_eq!(menu_entries(context)[1].is_enabled(), enabled, "{state:?}");
            assert_eq!(
                SessionAction::AiRename.hint_key(&context),
                Some(hint),
                "{state:?}"
            );
        }
    }

    #[test]
    fn move_items_explain_a_running_turn() {
        let busy = MenuContext {
            busy: true,
            ..ready()
        };
        assert_eq!(
            SessionAction::MoveToWorktree.hint_key(&busy),
            Some("sessions.sidebar.session.moveToWorktree.tooltipBusy")
        );
        assert_eq!(SessionAction::MoveToWorktree.hint_key(&ready()), None);
        assert_eq!(
            SessionAction::MoveSession.hint_key(&ready()),
            Some("sessions.moveSession.hint")
        );
    }

    #[test]
    fn the_pin_submenu_checks_pinned_and_offers_unpin_while_pinned() {
        let checked =
            |entry: &SubmenuEntry| matches!(entry, SubmenuEntry::Item(item) if item.checked);
        let unpinned = pin_submenu(false, true);
        assert_eq!(unpinned.len(), 1);
        assert_eq!(unpinned[0].action(), Some(&SubmenuAction::Pin));
        assert!(!checked(&unpinned[0]));
        let pinned = pin_submenu(true, true);
        assert!(
            pin_submenu(true, false)
                .iter()
                .all(|entry| !entry.is_enabled())
        );
        assert!(checked(&pinned[0]));
        assert_eq!(pinned[1].action(), Some(&SubmenuAction::Unpin));
    }

    #[test]
    fn highlight_skips_separators_and_disabled_items_and_wraps() {
        let entries = menu_entries(MenuContext {
            available: true,
            ..MenuContext::default()
        });
        // Rename, AI rename (checking), Copy ID, Pin to, Export, Side panel,
        // Mini Chat (no directory), separator, Archive, Delete.
        let last = entries.len() - 1;
        assert_eq!(step_highlight(&entries, None, 1), Some(0));
        assert_eq!(step_highlight(&entries, Some(0), 1), Some(2));
        assert_eq!(step_highlight(&entries, Some(last), 1), Some(0));
        assert_eq!(step_highlight(&entries, None, -1), Some(last));
        assert_eq!(step_highlight(&entries, Some(0), -1), Some(last));
        assert_eq!(edge_highlight(&entries, true), Some(last));
        let offline = menu_entries(MenuContext::default());
        assert_eq!(step_highlight(&offline, Some(2), 1), Some(3));
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
