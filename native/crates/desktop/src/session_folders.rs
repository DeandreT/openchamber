//! The shared session folders and the row menu's Move to folder.
//!
//! Follows `useSessionFoldersStore.ts`. The server stores the folders and is
//! the authority: what the sidebar shows is the server's last copy with this
//! window's changes that it has not stored yet laid over it. A change
//! applies here at once and goes to the server shortly after, in order and
//! one batch at a time. A copy older than the last one shown is ignored. A
//! batch the server rejects is dropped and the stored copy read again; a
//! failed one stays queued and is sent again with a growing delay, and on
//! the next event-stream connection.
//!
//! Unlike the web client, the copy and the queue live in memory only: a
//! change made while the server cannot be reached is lost when the app
//! quits before it is sent. Which folders are collapsed is this window's.

use std::collections::HashSet;
use std::time::Duration;

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::{AppContext, Context, Entity, EntityInputHandler, Subscription, Task, Window};
use openchamber_native_core::folders::{self, FolderChange, FoldersMap, NewFolder};
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_core::session_actions::SubmenuEntry;
use openchamber_native_core::session_moves;
use openchamber_native_transport::session_folders::{FolderSnapshot, FolderWrite};

use crate::workspace::Workspace;

/// `SERVER_WRITE_DEBOUNCE_MS`.
const WRITE_DEBOUNCE: Duration = Duration::from_millis(150);
/// The server takes at most 1,000 changes per request.
const MAX_CHANGES_PER_REQUEST: usize = 500;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
/// The name a new folder starts with, before the user renames it.
const NEW_FOLDER_NAME: &str = "New folder";

/// A folder name being edited.
pub(crate) struct FolderRename {
    pub(crate) scope: String,
    pub(crate) folder_id: String,
    pub(crate) input: Entity<InputState>,
    _subscription: Subscription,
}

/// The folders the sidebar shows and the changes on their way.
#[derive(Default)]
pub(crate) struct SessionFolders {
    pub(crate) map: FoldersMap,
    /// The revision of the last server copy shown.
    revision: i64,
    hydrated: bool,
    pending: Vec<FolderChange>,
    inflight: Vec<FolderChange>,
    retry_attempt: u32,
    /// The read, write or wait that runs; dropping it cancels it.
    task: Option<Task<()>>,
    /// A refresh asked for while another request ran.
    refresh_again: bool,
    runtime: Option<RuntimeId>,
}

impl SessionFolders {
    /// Shows a server copy with this window's queued changes on top. An
    /// older copy than the last one shown changes nothing.
    fn adopt(&mut self, snapshot: FolderSnapshot) -> bool {
        if snapshot.updated_at < self.revision {
            return false;
        }
        self.revision = snapshot.updated_at;
        let mut next = snapshot.folders;
        folders::apply_changes(&mut next, self.inflight.iter().chain(&self.pending));
        if next == self.map {
            return false;
        }
        self.map = next;
        true
    }

    /// Settles the batch in flight. The queue keeps the batch while it is
    /// on its way, so a stored batch leaves it and a failed one stays first.
    fn settle(&mut self, stored: bool) {
        let batch = std::mem::take(&mut self.inflight);
        if stored {
            self.pending.drain(..batch.len().min(self.pending.len()));
            self.retry_attempt = 0;
        }
    }

    fn retry_delay(&mut self) -> Duration {
        let delay = Duration::from_secs(1u64 << self.retry_attempt.min(5)).min(MAX_RETRY_DELAY);
        self.retry_attempt = self.retry_attempt.saturating_add(1);
        delay
    }
}

impl Workspace {
    /// The event stream (re)connected: anything may have changed meanwhile,
    /// and queued changes can go now.
    pub(crate) fn sync_session_folders(&mut self, cx: &mut Context<Self>) {
        let state = &mut self.session_management.folders;
        if state.runtime.as_ref() != Some(&self.runtime) {
            // Another runtime's folders and queue mean nothing here; its
            // in-flight write finishes against its own server.
            *state = SessionFolders {
                runtime: Some(self.runtime.clone()),
                ..SessionFolders::default()
            };
            self.sidebar_state.folders.clear();
            self.sidebar_state.collapsed_folders.clear();
        }
        self.read_session_folders(cx);
    }

    /// Another client changed the folders.
    pub(crate) fn session_folders_changed(&mut self, updated_at: i64, cx: &mut Context<Self>) {
        let state = &self.session_management.folders;
        if state.hydrated && updated_at <= state.revision {
            return;
        }
        self.read_session_folders(cx);
    }

    fn folder_request_running(&self) -> bool {
        self.session_management.folders.task.is_some()
    }

    /// Reads the stored copy. A request already running reads again after
    /// it.
    fn read_session_folders(&mut self, cx: &mut Context<Self>) {
        if self.folder_request_running() {
            self.session_management.folders.refresh_again = true;
            return;
        }
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let runtime = self.runtime.clone();
        let read = connection
            .network
            .run(async move { client.session_folders().await });
        self.session_management.folders.task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                if this.runtime != runtime {
                    return;
                }
                let state = &mut this.session_management.folders;
                state.task = None;
                match result {
                    Some(Ok(snapshot)) => {
                        let changed = state.adopt(snapshot);
                        state.hydrated = true;
                        state.retry_attempt = 0;
                        if changed {
                            this.folders_changed(cx);
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "the session folders could not be read");
                        if !this.session_management.folders.hydrated {
                            this.retry_session_folders(cx);
                            return;
                        }
                    }
                    None => return,
                }
                if std::mem::take(&mut this.session_management.folders.refresh_again) {
                    this.read_session_folders(cx);
                } else {
                    this.flush_session_folders(cx);
                }
            });
        }));
    }

    /// Applies a change here and queues it for the server; a change with no
    /// effect does neither.
    pub(crate) fn commit_folder_change(&mut self, change: FolderChange, cx: &mut Context<Self>) {
        let state = &mut self.session_management.folders;
        if !folders::apply_change(&mut state.map, &change) {
            return;
        }
        state.pending.push(change);
        self.folders_changed(cx);
        if self.session_management.folders.hydrated && !self.folder_request_running() {
            let runtime = self.runtime.clone();
            self.session_management.folders.task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(WRITE_DEBOUNCE).await;
                let _ = this.update(cx, |this, cx| {
                    if this.runtime == runtime {
                        this.session_management.folders.task = None;
                        this.flush_session_folders(cx);
                    }
                });
            }));
        }
    }

    /// Sends the next batch of queued changes.
    fn flush_session_folders(&mut self, cx: &mut Context<Self>) {
        let state = &self.session_management.folders;
        if !state.hydrated
            || self.folder_request_running()
            || !state.inflight.is_empty()
            || state.pending.is_empty()
        {
            return;
        }
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let count = state.pending.len().min(MAX_CHANGES_PER_REQUEST);
        let batch: Vec<FolderChange> = state.pending[..count].to_vec();
        let runtime = self.runtime.clone();
        let sent = batch.clone();
        let write = connection
            .network
            .run(async move { client.change_session_folders(&sent).await });
        self.session_management.folders.inflight = batch;
        self.session_management.folders.task = Some(cx.spawn(async move |this, cx| {
            let result = write.await;
            let _ = this.update(cx, |this, cx| {
                if this.runtime != runtime {
                    return;
                }
                let state = &mut this.session_management.folders;
                state.task = None;
                match result {
                    Some(Ok(FolderWrite::Stored(snapshot))) => {
                        state.settle(true);
                        if state.adopt(snapshot) {
                            this.folders_changed(cx);
                        }
                        this.flush_session_folders(cx);
                    }
                    Some(Ok(FolderWrite::Rejected)) => {
                        // The server cannot store these; show what it has.
                        state.settle(true);
                        this.read_session_folders(cx);
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "session folder changes could not be stored");
                        state.settle(false);
                        this.retry_session_folders(cx);
                    }
                    None => state.settle(false),
                }
            });
        }));
    }

    fn retry_session_folders(&mut self, cx: &mut Context<Self>) {
        let delay = self.session_management.folders.retry_delay();
        let runtime = self.runtime.clone();
        self.session_management.folders.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                if this.runtime != runtime {
                    return;
                }
                this.session_management.folders.task = None;
                if this.session_management.folders.hydrated {
                    this.flush_session_folders(cx);
                } else {
                    this.read_session_folders(cx);
                }
            });
        }));
    }

    /// The sidebar shows the new copy; collapse marks of folders that are
    /// gone are dropped.
    fn folders_changed(&mut self, cx: &mut Context<Self>) {
        let map = &self.session_management.folders.map;
        let known: HashSet<&str> = map
            .values()
            .flatten()
            .map(|folder| folder.id.as_str())
            .collect();
        self.sidebar_state
            .collapsed_folders
            .retain(|id| known.contains(id.as_str()));
        self.sidebar_state.folders = map.clone();
        self.rebuild_sidebar();
        self.refresh_session_submenu(
            openchamber_native_core::session_actions::Submenu::Folder,
            cx,
        );
        cx.notify();
    }

    /// The scopes `session` can be filed in.
    fn session_folder_scopes(&self, session: &SessionId) -> Vec<String> {
        let Some(directory) = self
            .sessions
            .get(session)
            .and_then(|record| record.resolved_directory())
        else {
            return Vec::new();
        };
        let project = session_moves::project_for_directory(
            &self.topology.projects,
            &self.topology.worktrees,
            &directory,
        );
        let worktrees: Vec<&str> = project
            .and_then(|project| {
                self.topology
                    .worktrees
                    .iter()
                    .find(|listing| listing.project_path == project.path)
            })
            .map(|listing| {
                listing
                    .worktrees
                    .iter()
                    .map(|worktree| worktree.path.as_str())
                    .collect()
            })
            .unwrap_or_default();
        let chats_root = self
            .topology
            .chat_roots
            .as_ref()
            .and_then(|roots| roots.root_of(Some(&directory)));
        folders::folder_scopes(
            project.map(|project| project.path.as_str()),
            worktrees,
            chats_root,
            &directory,
        )
    }

    pub(crate) fn folder_submenu_entries(&self, session: &SessionId) -> Vec<SubmenuEntry> {
        let scopes = self.session_folder_scopes(session);
        folders::folder_submenu(
            &scopes,
            &self.session_management.folders.map,
            session.as_str(),
        )
    }

    /// Files the session in a folder, or takes it out of the folder it is in.
    pub(crate) fn assign_session_folder(
        &mut self,
        session: &SessionId,
        scope: &str,
        folder_id: &str,
        cx: &mut Context<Self>,
    ) {
        let scopes = self.session_folder_scopes(session);
        let changes = folders::file_session(
            &scopes,
            &self.session_management.folders.map,
            scope,
            folder_id,
            session.as_str(),
        );
        for change in changes {
            self.commit_folder_change(change, cx);
        }
    }

    pub(crate) fn remove_session_from_folder(
        &mut self,
        session: &SessionId,
        scope: &str,
        cx: &mut Context<Self>,
    ) {
        self.commit_folder_change(
            FolderChange::Unassign {
                scope: scope.to_owned(),
                session_ids: vec![session.as_str().to_owned()],
            },
            cx,
        );
    }

    /// Creates a folder in `scope` holding the session, then opens its name
    /// for editing.
    pub(crate) fn create_folder_for_session(
        &mut self,
        session: &SessionId,
        scope: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = new_folder_id();
        self.commit_folder_change(
            FolderChange::Create {
                scope: scope.to_owned(),
                folder: NewFolder {
                    id: id.clone(),
                    name: NEW_FOLDER_NAME.to_owned(),
                    parent_id: None,
                    created_at: now_ms(),
                },
            },
            cx,
        );
        let scopes = self.session_folder_scopes(session);
        for other in scopes.iter().filter(|other| other.as_str() != scope) {
            self.commit_folder_change(
                FolderChange::Unassign {
                    scope: other.clone(),
                    session_ids: vec![session.as_str().to_owned()],
                },
                cx,
            );
        }
        self.commit_folder_change(
            FolderChange::Assign {
                scope: scope.to_owned(),
                folder_id: id.clone(),
                session_ids: vec![session.as_str().to_owned()],
            },
            cx,
        );
        self.start_folder_rename(scope.to_owned(), id, window, cx);
    }

    /// Opens a folder's name for editing with the whole name selected.
    pub(crate) fn start_folder_rename(
        &mut self,
        scope: String,
        folder_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self
            .session_management
            .folders
            .map
            .get(&scope)
            .and_then(|list| list.iter().find(|folder| folder.id == folder_id))
            .map(|folder| folder.name.clone())
        else {
            return;
        };
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |input, cx| {
            input.set_value(name, window, cx);
            input.select_all(window, cx);
            input.focus(window, cx);
        });
        let subscription = cx.subscribe_in(&input, window, |this, input, event, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                let composing = input.update(cx, |input, cx| {
                    input.marked_text_range(window, cx).is_some()
                });
                if !composing {
                    this.save_folder_rename(window, cx);
                }
            }
        });
        // A collapsed folder opens so the field shows.
        self.sidebar_state.collapsed_folders.remove(&folder_id);
        self.session_management.folder_rename = Some(FolderRename {
            scope,
            folder_id,
            input,
            _subscription: subscription,
        });
        self.rebuild_sidebar();
        cx.notify();
    }

    /// Commits a trimmed, nonempty name; an empty one keeps the old name.
    pub(crate) fn save_folder_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.session_management.folder_rename.take() else {
            return;
        };
        let name = rename.input.read(cx).value().trim().to_owned();
        if !name.is_empty() {
            self.commit_folder_change(
                FolderChange::Rename {
                    scope: rename.scope,
                    folder_id: rename.folder_id,
                    name,
                },
                cx,
            );
        }
        self.focus_composer(window, cx);
        cx.notify();
    }

    pub(crate) fn cancel_folder_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_management.folder_rename.take().is_some() {
            self.focus_composer(window, cx);
            cx.notify();
        }
    }

    /// Opens or closes a folder's sessions.
    pub(crate) fn toggle_folder(&mut self, folder_id: &str, cx: &mut Context<Self>) {
        let collapsed = &mut self.sidebar_state.collapsed_folders;
        if !collapsed.remove(folder_id) {
            collapsed.insert(folder_id.to_owned());
        }
        self.rebuild_sidebar();
        cx.notify();
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

impl Workspace {
    /// A folder's inline name field, while that folder is being renamed.
    pub(crate) fn render_folder_rename(
        &self,
        folder_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<gpui_kit::AnyElement> {
        use gpui_kit::{InteractiveElement, IntoElement, ParentElement, Styled};

        let rename = self
            .session_management
            .folder_rename
            .as_ref()
            .filter(|rename| rename.folder_id == folder_id)?;
        let input = rename.input.clone();
        Some(
            gpui_kit::div()
                .id("folder-rename")
                .flex_1()
                .min_w_0()
                .capture_action(cx.listener(
                    move |this, _: &gpui_kit::component::input::Escape, window, cx| {
                        let composing = input.update(cx, |input, cx| {
                            input.marked_text_range(window, cx).is_some()
                        });
                        if !composing {
                            cx.stop_propagation();
                            this.cancel_folder_rename(window, cx);
                        }
                    },
                ))
                .on_mouse_down_out(
                    cx.listener(|this, _, window, cx| this.save_folder_rename(window, cx)),
                )
                .child(
                    gpui_kit::component::input::Input::new(&rename.input)
                        .appearance(false)
                        .bordered(false)
                        .text_size(gpui_kit::rems(0.843_75)),
                )
                .into_any_element(),
        )
    }
}

/// `createFolderId`: a random version 4 UUID.
fn new_folder_id() -> String {
    use std::hash::{BuildHasher, Hasher};

    let mut bytes = [0u8; 16];
    for (index, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_usize(index);
        hasher.write_i64(now_ms());
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use openchamber_native_core::folders::SessionFolder;

    fn folder(id: &str, sessions: &[&str]) -> SessionFolder {
        SessionFolder {
            id: id.to_owned(),
            name: id.to_owned(),
            session_ids: sessions.iter().map(|id| (*id).to_owned()).collect(),
            created_at: 0,
            parent_id: None,
        }
    }

    fn snapshot(updated_at: i64, folders: Vec<SessionFolder>) -> FolderSnapshot {
        FolderSnapshot {
            folders: FoldersMap::from([("/p".to_owned(), folders)]),
            updated_at,
        }
    }

    #[test]
    fn queued_changes_lie_over_every_newer_server_copy() {
        let mut state = SessionFolders::default();
        assert!(state.adopt(snapshot(1, vec![folder("a", &[])])));
        let change = FolderChange::Assign {
            scope: "/p".into(),
            folder_id: "a".into(),
            session_ids: vec!["s".into()],
        };
        folders::apply_change(&mut state.map, &change);
        state.pending.push(change);
        // A newer copy that does not have the change yet keeps showing it.
        state.adopt(snapshot(2, vec![folder("a", &[]), folder("b", &[])]));
        assert_eq!(state.map["/p"][0].session_ids, ["s"]);
        assert_eq!(state.map["/p"].len(), 2);
        assert!(
            !state.adopt(snapshot(1, vec![])),
            "an older copy is ignored"
        );
    }

    #[test]
    fn a_failed_batch_stays_in_front_of_newer_changes() {
        let mut state = SessionFolders::default();
        let change = |name: &str| FolderChange::Rename {
            scope: "/p".into(),
            folder_id: "a".into(),
            name: name.into(),
        };
        state.pending = vec![change("one"), change("two")];
        state.inflight = vec![change("one")];
        state.settle(false);
        assert_eq!(state.pending, [change("one"), change("two")]);
        assert!(state.inflight.is_empty());
        state.inflight = vec![change("one")];
        state.settle(true);
        assert_eq!(state.pending, [change("two")]);
    }

    #[test]
    fn retries_back_off_to_thirty_seconds() {
        let mut state = SessionFolders::default();
        let delays: Vec<u64> = (0..7).map(|_| state.retry_delay().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
    }

    #[test]
    fn folder_ids_are_version_four_uuids() {
        let id = new_folder_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_ne!(id, new_folder_id());
    }
}
