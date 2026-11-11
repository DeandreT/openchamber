//! The MCP servers of the directory being worked in, with their connection
//! status and on/off switch (`useMcpStore`, `useMcpDirectory`).
//!
//! Status belongs to the directory that runs the turn: the selected
//! session's, or the draft's chosen project before the first send. Without
//! one nothing is read, since a read without a directory would start
//! OpenCode's own working directory and its servers. A directory is read
//! when the work status card first shows its MCP section and its status is
//! missing or older than a minute; after that OpenCode's
//! `mcp.status.changed` announcements re-read it, once per burst. A failed
//! read keeps the servers already held and shows nothing new.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui_kit::component::notification::Notification;
use gpui_kit::{Context, Task, Window};
use openchamber_native_core::path;
use openchamber_native_transport::mcp::{McpConnection, McpServerStatus};

use crate::context_panel::work_status_sections::Layout;
use crate::locale;
use crate::workspace::Workspace;

/// A status older than this is read again when the section shows
/// (`MCP_STATUS_MAX_AGE_MS`).
const MAX_AGE: Duration = Duration::from_secs(60);
/// Status announcements arrive in bursts while a location starts; one
/// re-read follows once the burst settles (`MCP_STATUS_REFRESH_DEBOUNCE_MS`).
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);

/// `localeCompare` for the ASCII names servers carry: case folds first, then
/// the original spelling breaks the tie.
fn by_name(a: &McpServerStatus, b: &McpServerStatus) -> Ordering {
    a.name
        .to_lowercase()
        .cmp(&b.name.to_lowercase())
        .then_with(|| b.name.cmp(&a.name))
}

/// The read a caller started with [`McpStatusStore::begin_refresh`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefreshTicket {
    directory: String,
    generation: u64,
    forget_generation: u64,
}

/// Each directory's servers, as last read successfully.
#[derive(Debug, Default)]
pub(crate) struct McpStatusStore {
    /// Sorted by name. An entry with no servers is an answered read; a
    /// directory never read successfully has no entry.
    held: HashMap<String, Vec<McpServerStatus>>,
    refreshed_at: HashMap<String, Instant>,
    /// Directories whose newest read failed.
    failed: HashSet<String>,
    /// Bumped on every runtime switch. Status is keyed by directory alone and
    /// two servers can hold the same path, so a read on its way for the
    /// previous one must not land.
    generation: u64,
    /// Bumped per directory when its status is forgotten, so a read that
    /// started before OpenCode stopped the directory cannot write it back.
    forget_generations: HashMap<String, u64>,
}

impl McpStatusStore {
    /// The directory's servers by name; empty before a successful read.
    pub(crate) fn servers(&self, directory: &str) -> &[McpServerStatus] {
        self.held
            .get(directory)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Whether a read of the directory ever succeeded and was not forgotten.
    pub(crate) fn is_held(&self, directory: &str) -> bool {
        self.held.contains_key(directory)
    }

    /// Whether the directory's newest read failed. Its servers, when held,
    /// are the last ones read successfully.
    #[cfg(test)]
    pub(crate) fn last_refresh_failed(&self, directory: &str) -> bool {
        self.failed.contains(directory)
    }

    /// Whether the last successful read is younger than `max_age` at `now`.
    pub(crate) fn is_fresh(&self, directory: &str, now: Instant, max_age: Duration) -> bool {
        self.refreshed_at
            .get(directory)
            .is_some_and(|read| now.saturating_duration_since(*read) < max_age)
    }

    /// Names the read to finish with [`Self::finish_refresh`].
    pub(crate) fn begin_refresh(&self, directory: &str) -> RefreshTicket {
        RefreshTicket {
            directory: directory.to_owned(),
            generation: self.generation,
            forget_generation: self.forget_generation(directory),
        }
    }

    fn forget_generation(&self, directory: &str) -> u64 {
        self.forget_generations
            .get(directory)
            .copied()
            .unwrap_or_default()
    }

    /// Takes a read's answer, `None` when it failed. Returns whether the
    /// servers shown changed. A read from before a runtime switch or from
    /// before its directory was forgotten changes nothing; a failed read
    /// keeps the servers held.
    pub(crate) fn finish_refresh(
        &mut self,
        ticket: &RefreshTicket,
        servers: Option<Vec<McpServerStatus>>,
        now: Instant,
    ) -> bool {
        if ticket.generation != self.generation
            || ticket.forget_generation != self.forget_generation(&ticket.directory)
        {
            return false;
        }
        let Some(mut servers) = servers else {
            self.failed.insert(ticket.directory.clone());
            return false;
        };
        servers.sort_by(by_name);
        self.failed.remove(&ticket.directory);
        self.refreshed_at.insert(ticket.directory.clone(), now);
        let changed = self.servers(&ticket.directory) != servers.as_slice();
        self.held.insert(ticket.directory.clone(), servers);
        changed
    }

    /// Forgets a directory OpenCode stopped: the held status describes
    /// servers that are gone. Returns whether servers were held.
    pub(crate) fn forget(&mut self, directory: &str) -> bool {
        *self
            .forget_generations
            .entry(directory.to_owned())
            .or_default() += 1;
        self.refreshed_at.remove(directory);
        self.failed.remove(directory);
        self.held.remove(directory).is_some()
    }

    /// Forgets every directory, as a new runtime must.
    pub(crate) fn reset(&mut self) {
        self.generation += 1;
        self.held.clear();
        self.refreshed_at.clear();
        self.failed.clear();
        self.forget_generations.clear();
    }
}

/// What the card's MCP section knows and remembers.
#[derive(Default)]
pub(crate) struct McpStatus {
    pub(crate) store: McpStatusStore,
    /// The read in flight per directory; a newer read replaces it.
    reads: HashMap<String, Task<()>>,
    /// The pending re-read per directory after status announcements.
    debounced: HashMap<String, Task<()>>,
    /// The servers whose switch is in flight, each with its directory. No
    /// second switch goes out for one of them meanwhile.
    busy: HashSet<(String, String)>,
    /// What the section last asked for: the directory and the connection
    /// attempt. A missing or stale status is read only when this changes,
    /// never on every frame.
    asked: Option<(String, u64)>,
    /// Directories on screen that OpenCode stopped while the window was
    /// inactive. Their status is not re-read until the user returns, or an
    /// idle app would restart the directory after every idle shutdown.
    dormant: HashSet<String>,
}

impl McpStatus {
    /// Whether `name`'s switch in `directory` is in flight.
    pub(crate) fn is_busy(&self, directory: &str, name: &str) -> bool {
        self.busy
            .iter()
            .any(|(busy_directory, busy)| busy_directory == directory && busy == name)
    }

    fn forget(&mut self, directory: &str) -> bool {
        self.debounced.remove(directory);
        self.reads.remove(directory);
        self.store.forget(directory)
    }
}

impl Workspace {
    /// The directory MCP connections belong to: the selected session's, or
    /// the project the draft's first send will run in. `None` while neither
    /// is known, and then nothing is read or changed.
    pub(crate) fn mcp_directory(&self) -> Option<String> {
        self.project_subject().map(|subject| subject.directory)
    }

    /// Reads the directory's status when the card's MCP section starts
    /// showing it, and wakes directories that went dormant. Called while
    /// rendering, so every way the directory or the card changes is covered.
    pub(crate) fn ensure_mcp_status(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.mcp_status.dormant.is_empty() && window.is_window_active() {
            self.wake_dormant_mcp(cx);
        }
        let shown = (self.work_status_inline(window) || self.work_status_overlay)
            && Layout::for_workspace(self, cx)
                .visible_builtin_order()
                .any(|id| id == "mcp");
        let attempt = self
            .connection
            .as_ref()
            .filter(|connection| connection.client.is_some())
            .map(|connection| connection.generation);
        let asked = shown.then(|| self.mcp_directory()).flatten().zip(attempt);
        if self.mcp_status.asked == asked {
            return;
        }
        self.mcp_status.asked = asked.clone();
        let Some((directory, _)) = asked else {
            return;
        };
        if self
            .mcp_status
            .store
            .is_fresh(&directory, Instant::now(), MAX_AGE)
            || self.mcp_status.reads.contains_key(&directory)
        {
            return;
        }
        self.refresh_mcp_status(&directory, cx);
    }

    /// Reads `directory`'s servers again. A failure keeps what is held.
    pub(crate) fn refresh_mcp_status(&mut self, directory: &str, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        let runtime = self.runtime.clone();
        let ticket = self.mcp_status.store.begin_refresh(directory);
        let read_directory = directory.to_owned();
        let read = connection
            .network
            .run(async move { client.mcp_servers(&read_directory).await });
        let key = directory.to_owned();
        let task = cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                // A newer read would have replaced this task before it ran.
                this.mcp_status.reads.remove(&key);
                if this.runtime != runtime {
                    return;
                }
                let servers = match result {
                    Some(Ok(servers)) => Some(servers),
                    Some(Err(error)) => {
                        tracing::warn!(%error, "the MCP servers could not be read");
                        None
                    }
                    None => None,
                };
                if this
                    .mcp_status
                    .store
                    .finish_refresh(&ticket, servers, Instant::now())
                {
                    cx.notify();
                }
            });
        });
        self.mcp_status.reads.insert(directory.to_owned(), task);
    }

    /// OpenCode announced a status change in `directory`. A directory whose
    /// status is held is re-read once the burst settles; one nobody asked
    /// about is left alone, since reading it would start it.
    pub(crate) fn mcp_status_changed(&mut self, directory: Option<&str>, cx: &mut Context<Self>) {
        let Some(directory) = directory.and_then(path::normalize) else {
            return;
        };
        if self.mcp_status.dormant.contains(&directory) {
            return;
        }
        let key = directory.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                this.mcp_status.debounced.remove(&key);
                if this.mcp_status.store.is_held(&key) {
                    this.refresh_mcp_status(&key, cx);
                }
            });
        });
        self.mcp_status.debounced.insert(directory, task);
    }

    /// OpenCode stopped `directory`'s services. Another directory's status
    /// is forgotten: its servers are gone, and the announcements OpenCode
    /// makes while stopping them would otherwise re-read the directory and
    /// start it again. The directory on screen keeps its status; with the
    /// window inactive it waits for the user before any re-read.
    pub(crate) fn mcp_location_shutdown(
        &mut self,
        directory: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(directory) = path::normalize(directory) else {
            return;
        };
        if self.mcp_directory().as_deref() != Some(directory.as_str()) {
            if self.mcp_status.forget(&directory) {
                cx.notify();
            }
        } else if window.is_window_active() {
            self.mcp_status.dormant.remove(&directory);
        } else {
            self.mcp_status.debounced.remove(&directory);
            self.mcp_status.dormant.insert(directory);
        }
    }

    /// The user is back: a dormant directory still on screen is re-read if
    /// its status is held, and any other is forgotten.
    fn wake_dormant_mcp(&mut self, cx: &mut Context<Self>) {
        let current = self.mcp_directory();
        for directory in std::mem::take(&mut self.mcp_status.dormant) {
            if current.as_deref() != Some(directory.as_str()) {
                self.mcp_status.forget(&directory);
            } else if self.mcp_status.store.is_held(&directory) {
                self.refresh_mcp_status(&directory, cx);
            }
        }
    }

    /// Forgets every directory's status, as a new runtime must. The section
    /// keeps the way the user left it folded.
    pub(crate) fn reset_mcp_status(&mut self) {
        self.mcp_status = McpStatus {
            store: std::mem::take(&mut self.mcp_status.store),
            ..McpStatus::default()
        };
        self.mcp_status.store.reset();
    }

    /// Says what a server waiting for sign-in needs. OpenCode owns the
    /// sign-in, so connecting would only repeat the attempt that asked for
    /// it.
    pub(crate) fn report_mcp_needs_auth(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = locale::strings(cx).get("chat.workStatus.mcp.needsAuthHint");
        self.push_notice(Notification::info(text.clone()), text, window, cx);
    }

    /// The on/off switch of `name` in the directory being worked in. The
    /// switch holds until the server answers, and a second press meanwhile
    /// sends nothing. The status is then read again whatever the outcome:
    /// a switch that failed or was cut short may still have written the
    /// config or reached the server.
    pub(crate) fn set_mcp_server_enabled(
        &mut self,
        name: &str,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(directory) = self.mcp_directory() else {
            return;
        };
        let waits_for_sign_in = self
            .mcp_status
            .store
            .servers(&directory)
            .iter()
            .any(|server| server.name == name && server.connection == McpConnection::NeedsAuth);
        if enabled && waits_for_sign_in {
            self.report_mcp_needs_auth(window, cx);
            return;
        }
        if self.mcp_status.is_busy(&directory, name) {
            return;
        }
        let runtime = self.runtime.clone();
        let sent = (directory.clone(), name.to_owned());
        let finished = sent.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    client
                        .set_mcp_server_enabled(&sent.1, enabled, &sent.0)
                        .await
                })
            },
            move |this, result, _, cx| {
                if this.runtime != runtime {
                    return;
                }
                this.mcp_status.busy.remove(&finished);
                if let Some(Err(error)) = result {
                    tracing::warn!(%error, "the MCP server could not be switched");
                }
                this.refresh_mcp_status(&finished.0, cx);
            },
            window,
            cx,
        );
        if started {
            self.mcp_status.busy.insert((directory, name.to_owned()));
            cx.notify();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::sync::{Arc, Mutex};

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext};
    use openchamber_native_core::ids::RuntimeId;
    use openchamber_native_transport::client::ServerClient;
    use openchamber_native_transport::credential::Credential;
    use serde_json::{Value, json};

    use crate::connection::{Connection, ServerConfig};
    use crate::settings_test_server::{Request, Server, settle};

    use super::*;

    fn server(name: &str, connection: McpConnection) -> McpServerStatus {
        McpServerStatus {
            name: name.to_owned(),
            connection,
        }
    }

    fn names(store: &McpStatusStore, directory: &str) -> Vec<String> {
        store
            .servers(directory)
            .iter()
            .map(|server| server.name.clone())
            .collect()
    }

    #[test]
    fn servers_list_by_name_and_an_empty_answer_is_held() {
        let mut store = McpStatusStore::default();
        let now = Instant::now();
        let ticket = store.begin_refresh("/repo");
        assert!(store.finish_refresh(
            &ticket,
            Some(vec![
                server("zeta", McpConnection::Connected),
                server("Alpha", McpConnection::Failed),
                server("beta", McpConnection::Disabled),
            ]),
            now,
        ));
        assert_eq!(names(&store, "/repo"), ["Alpha", "beta", "zeta"]);
        assert!(store.is_fresh("/repo", now + Duration::from_secs(59), MAX_AGE));
        assert!(!store.is_fresh("/repo", now + Duration::from_secs(60), MAX_AGE));

        let ticket = store.begin_refresh("/empty");
        assert!(
            !store.finish_refresh(&ticket, Some(Vec::new()), now),
            "nothing is shown either way"
        );
        assert!(store.is_held("/empty"), "an empty answer is an answer");
        assert!(store.is_fresh("/empty", now, MAX_AGE));
    }

    #[test]
    fn a_failed_first_read_holds_nothing_and_is_not_fresh() {
        let mut store = McpStatusStore::default();
        let ticket = store.begin_refresh("/repo");
        assert!(!store.finish_refresh(&ticket, None, Instant::now()));
        assert!(!store.is_held("/repo"), "a failure is not an empty answer");
        assert!(store.last_refresh_failed("/repo"));
        assert!(
            !store.is_fresh("/repo", Instant::now(), MAX_AGE),
            "the next time the section shows, it asks again"
        );
    }

    #[test]
    fn a_failed_reread_keeps_the_servers_and_one_directory_does_not_touch_another() {
        let mut store = McpStatusStore::default();
        let now = Instant::now();
        let ticket = store.begin_refresh("/repo");
        store.finish_refresh(
            &ticket,
            Some(vec![server("a", McpConnection::Connected)]),
            now,
        );
        let ticket = store.begin_refresh("/other");
        store.finish_refresh(
            &ticket,
            Some(vec![server("b", McpConnection::Pending)]),
            now,
        );

        let ticket = store.begin_refresh("/repo");
        assert!(!store.finish_refresh(&ticket, None, now + MAX_AGE));
        assert_eq!(names(&store, "/repo"), ["a"]);
        assert!(store.last_refresh_failed("/repo"));
        assert!(
            !store.is_fresh("/repo", now + MAX_AGE, MAX_AGE),
            "a failed reread does not renew the age"
        );
        assert_eq!(names(&store, "/other"), ["b"]);
        assert!(!store.last_refresh_failed("/other"));

        let ticket = store.begin_refresh("/repo");
        assert!(store.finish_refresh(
            &ticket,
            Some(vec![server("a", McpConnection::Disabled)]),
            now + MAX_AGE,
        ));
        assert!(!store.last_refresh_failed("/repo"));
    }

    #[test]
    fn reads_from_a_previous_runtime_or_a_forgotten_directory_never_land() {
        let mut store = McpStatusStore::default();
        let now = Instant::now();
        let ticket = store.begin_refresh("/repo");
        store.finish_refresh(
            &ticket,
            Some(vec![server("a", McpConnection::Connected)]),
            now,
        );

        let retired = store.begin_refresh("/repo");
        store.reset();
        assert!(!store.is_held("/repo"));
        assert!(!store.finish_refresh(
            &retired,
            Some(vec![server("previous", McpConnection::Connected)]),
            now,
        ));
        assert!(!store.is_held("/repo"), "the same path on another server");

        let ticket = store.begin_refresh("/repo");
        store.finish_refresh(
            &ticket,
            Some(vec![server("a", McpConnection::Connected)]),
            now,
        );
        let stopped = store.begin_refresh("/repo");
        assert!(store.forget("/repo"));
        assert!(!store.finish_refresh(
            &stopped,
            Some(vec![server("a", McpConnection::Connected)]),
            now,
        ));
        assert!(
            !store.is_held("/repo"),
            "a read from before the shutdown cannot write the status back"
        );
        assert!(!store.forget("/never-read"));
    }

    struct Fixture {
        server: Server,
        fail_status: Arc<AtomicBool>,
        fail_config: Arc<AtomicBool>,
        connected: Arc<Mutex<Vec<(&'static str, &'static str)>>>,
    }

    impl Fixture {
        /// Serves `servers` as (name, status) for every directory and
        /// accepts every switch.
        fn new(servers: Vec<(&'static str, &'static str)>) -> Self {
            let fail_status = Arc::new(AtomicBool::new(false));
            let fail_config = Arc::new(AtomicBool::new(false));
            let connected = Arc::new(Mutex::new(servers));
            let (status, config, held) =
                (fail_status.clone(), fail_config.clone(), connected.clone());
            let server = Server::new(move |request| {
                if request.is("GET", "/api/mcp") {
                    if status.load(AtomicOrdering::Acquire) {
                        return (503, json!({"error": "starting"}));
                    }
                    let data: Vec<Value> = held
                        .lock()
                        .expect("servers")
                        .iter()
                        .map(|(name, state)| json!({"name": name, "status": {"status": state}}))
                        .collect();
                    return (200, json!({"location": {}, "data": data}));
                }
                if request.is("GET", "/api/config/mcp") {
                    return if config.load(AtomicOrdering::Acquire) {
                        (500, json!({"error": "unreadable"}))
                    } else {
                        (200, json!([]))
                    };
                }
                if request.method == "POST" && request.target.ends_with("/disconnect") {
                    for entry in held.lock().expect("servers").iter_mut() {
                        if request.target.contains(&format!("/mcp/{}/", entry.0)) {
                            entry.1 = "disabled";
                        }
                    }
                    return (200, json!({}));
                }
                (404, json!({"error": "unknown route"}))
            });
            Self {
                server,
                fail_status,
                fail_config,
                connected,
            }
        }

        fn count(&self, matches: impl Fn(&Request) -> bool) -> usize {
            self.server
                .requests()
                .iter()
                .filter(|request| matches(request))
                .count()
        }

        fn status_reads(&self) -> usize {
            self.count(|request| request.is("GET", "/api/mcp"))
        }
    }

    /// A workspace connected to `server` whose draft runs in the first
    /// project. The settings document stays unread, so the composer's
    /// catalogs, which read `/api/mcp` for their own count, load nothing and
    /// every status read the server records is this owner's.
    pub(crate) fn connect(
        server: &Server,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>, String) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        let directory = cx
            .update_window(window, |_, _, cx| {
                workspace.update(cx, |workspace, _| {
                    let mut connection =
                        Connection::new(ServerConfig::for_endpoint(server.endpoint.clone()))
                            .expect("network");
                    connection.client = Some(
                        ServerClient::new(server.endpoint.clone(), Credential::None)
                            .expect("fixture client"),
                    );
                    workspace.connection = Some(connection);
                    workspace.send_catalog.settings = None;
                    workspace.selected = None;
                    let directory = workspace.topology.projects[0].path.clone();
                    workspace
                        .draft_projects
                        .insert(workspace.runtime.clone(), directory.clone());
                    directory
                })
            })
            .expect("test window");
        // The frame that first shows the card with this connection asks for
        // the directory's status.
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .expect("test window");
        (window, workspace, directory)
    }

    fn shown(
        workspace: &Entity<Workspace>,
        directory: &str,
        cx: &mut TestAppContext,
    ) -> Vec<(String, McpConnection)> {
        cx.read(|cx| {
            workspace
                .read(cx)
                .mcp_status
                .store
                .servers(directory)
                .iter()
                .map(|server| (server.name.clone(), server.connection))
                .collect()
        })
    }

    fn refresh(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        directory: &str,
        cx: &mut TestAppContext,
    ) {
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.refresh_mcp_status(directory, cx);
            });
        })
        .expect("test window");
        settle(window, cx);
    }

    #[gpui_kit::test]
    fn the_shown_section_reads_its_directory_once_and_a_failed_reread_keeps_rows(
        cx: &mut TestAppContext,
    ) {
        let fixture = Fixture::new(vec![("zeta", "connected"), ("alpha", "failed")]);
        let (window, workspace, directory) = connect(&fixture.server, cx);
        cx.read(|cx| {
            assert_eq!(
                workspace.read(cx).mcp_directory().as_deref(),
                Some(directory.as_str()),
                "the draft's project is the directory its first send runs in"
            );
        });
        // Rendering shows the card and asks once, however many frames follow.
        settle(window, cx);
        settle(window, cx);
        assert_eq!(fixture.status_reads(), 1);
        assert_eq!(
            shown(&workspace, &directory, cx),
            [
                ("alpha".to_owned(), McpConnection::Failed),
                ("zeta".to_owned(), McpConnection::Connected)
            ]
        );

        fixture.fail_status.store(true, AtomicOrdering::Release);
        refresh(window, &workspace, &directory, cx);
        assert_eq!(fixture.status_reads(), 2);
        assert_eq!(shown(&workspace, &directory, cx).len(), 2);
        cx.read(|cx| {
            let status = &workspace.read(cx).mcp_status.store;
            assert!(status.last_refresh_failed(&directory));
            assert!(status.is_held(&directory));
        });
    }

    #[gpui_kit::test]
    fn a_failed_first_read_shows_nothing_and_an_unresolved_draft_reads_nothing(
        cx: &mut TestAppContext,
    ) {
        let fixture = Fixture::new(vec![("alpha", "connected")]);
        fixture.fail_status.store(true, AtomicOrdering::Release);
        let (window, workspace, directory) = connect(&fixture.server, cx);
        settle(window, cx);
        assert_eq!(fixture.status_reads(), 1);
        cx.read(|cx| {
            let status = &workspace.read(cx).mcp_status.store;
            assert!(
                !status.is_held(&directory),
                "a failure is not an empty list"
            );
            assert!(status.last_refresh_failed(&directory));
        });

        // Without a project the draft has no directory: nothing is read and
        // the switch does nothing.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.draft_projects.clear();
                assert_eq!(workspace.mcp_directory(), None);
                workspace.set_mcp_server_enabled("alpha", false, window, cx);
            });
        })
        .expect("test window");
        settle(window, cx);
        assert_eq!(fixture.status_reads(), 1);
        assert_eq!(fixture.count(|request| request.method != "GET"), 0);
    }

    #[gpui_kit::test]
    fn announcements_reread_a_held_directory_once_and_leave_others_alone(cx: &mut TestAppContext) {
        let fixture = Fixture::new(vec![("alpha", "pending")]);
        let (window, workspace, directory) = connect(&fixture.server, cx);
        settle(window, cx);
        assert_eq!(fixture.status_reads(), 1);

        *fixture.connected.lock().expect("servers") = vec![("alpha", "connected")];
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                for _ in 0..3 {
                    workspace.mcp_status_changed(Some(&directory), cx);
                }
                workspace.mcp_status_changed(Some("/nobody/asked"), cx);
                workspace.mcp_status_changed(None, cx);
            });
        })
        .expect("test window");
        cx.executor().advance_clock(REFRESH_DEBOUNCE);
        settle(window, cx);
        assert_eq!(fixture.status_reads(), 2, "one re-read for the burst");
        assert_eq!(
            shown(&workspace, &directory, cx),
            [("alpha".to_owned(), McpConnection::Connected)]
        );
    }

    #[gpui_kit::test]
    fn a_runtime_switch_and_a_shutdown_elsewhere_discard_status_and_late_reads(
        cx: &mut TestAppContext,
    ) {
        let fixture = Fixture::new(vec![("alpha", "connected")]);
        let (window, workspace, directory) = connect(&fixture.server, cx);
        settle(window, cx);
        assert_eq!(shown(&workspace, &directory, cx).len(), 1);

        // Another directory's status is held, then OpenCode stops it while a
        // read of it is on its way.
        refresh(window, &workspace, "/background", cx);
        assert_eq!(shown(&workspace, "/background", cx).len(), 1);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.refresh_mcp_status("/background", cx);
                workspace.mcp_location_shutdown("/background", window, cx);
                // Its own announcements while stopping re-read nothing.
                workspace.mcp_status_changed(Some("/background"), cx);
            });
        })
        .expect("test window");
        let reads = fixture.status_reads();
        cx.executor().advance_clock(REFRESH_DEBOUNCE);
        settle(window, cx);
        assert!(shown(&workspace, "/background", cx).is_empty());
        assert!(
            fixture.status_reads() <= reads + 1,
            "at most the read already sent"
        );
        assert_eq!(
            shown(&workspace, &directory, cx).len(),
            1,
            "the directory on screen keeps its servers"
        );

        // A read is on its way while the runtime changes.
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.refresh_mcp_status(&directory, cx);
                workspace.runtime = RuntimeId::from("another-runtime");
                workspace.reset_mcp_status();
            });
        })
        .expect("test window");
        cx.read(|cx| assert!(!workspace.read(cx).mcp_status.store.is_held(&directory)));
    }

    #[gpui_kit::test]
    fn the_switch_holds_until_answered_and_every_outcome_rereads_the_status(
        cx: &mut TestAppContext,
    ) {
        let fixture = Fixture::new(vec![("alpha", "connected"), ("auth", "needs_auth")]);
        let (window, workspace, directory) = connect(&fixture.server, cx);
        settle(window, cx);
        let switch = |name: &'static str, enabled: bool, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.set_mcp_server_enabled(name, enabled, window, cx);
                });
            })
            .expect("test window");
        };

        // Off: the switch is busy, the server is disconnected, the status is
        // read again and the other server is untouched.
        switch("alpha", false, cx);
        cx.read(|cx| assert!(workspace.read(cx).mcp_status.is_busy(&directory, "alpha")));
        settle(window, cx);
        cx.read(|cx| assert!(!workspace.read(cx).mcp_status.is_busy(&directory, "alpha")));
        assert_eq!(
            shown(&workspace, &directory, cx),
            [
                ("alpha".to_owned(), McpConnection::Disabled),
                ("auth".to_owned(), McpConnection::NeedsAuth)
            ]
        );
        assert_eq!(
            fixture.count(|request| request.method == "POST"
                && request
                    .target
                    .starts_with("/api/experimental/mcp/alpha/disconnect")),
            1
        );

        // A server waiting for sign-in is not connected: the user is told
        // where to sign in and no request goes out.
        let writes = fixture.count(|request| request.method != "GET");
        let config_reads = fixture.count(|request| request.is("GET", "/api/config/mcp"));
        switch("auth", true, cx);
        settle(window, cx);
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            assert!(workspace.session_management.notice.is_some());
            assert!(!workspace.mcp_status.is_busy(&directory, "auth"));
        });
        assert_eq!(fixture.count(|request| request.method != "GET"), writes);
        assert_eq!(
            fixture.count(|request| request.is("GET", "/api/config/mcp")),
            config_reads
        );

        // A failed switch frees the control, says nothing and reads the
        // status again: any of its steps may have reached the server.
        fixture.fail_config.store(true, AtomicOrdering::Release);
        let reads = fixture.status_reads();
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, _| {
                workspace.session_management.notice = None
            });
        })
        .expect("test window");
        switch("alpha", true, cx);
        settle(window, cx);
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            assert!(!workspace.mcp_status.is_busy(&directory, "alpha"));
            assert!(
                workspace.session_management.notice.is_none(),
                "a failed switch stays silent"
            );
            assert!(!workspace.mcp_status.store.last_refresh_failed(&directory));
        });
        assert_eq!(
            fixture.status_reads(),
            reads + 1,
            "the rows come from a read made after the failure"
        );
        assert_eq!(fixture.count(|request| request.method != "GET"), writes);
        assert_eq!(
            shown(&workspace, &directory, cx),
            [
                ("alpha".to_owned(), McpConnection::Disabled),
                ("auth".to_owned(), McpConnection::NeedsAuth)
            ]
        );
    }

    #[gpui_kit::test]
    fn a_second_press_waits_for_the_servers_first_switch(cx: &mut TestAppContext) {
        let fixture = Fixture::new(vec![("alpha", "connected"), ("beta", "connected")]);
        let (window, workspace, directory) = connect(&fixture.server, cx);
        settle(window, cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_mcp_server_enabled("alpha", false, window, cx);
                // The first is still on its way: neither a repeat nor the
                // opposite switch goes out for the same server.
                workspace.set_mcp_server_enabled("alpha", false, window, cx);
                workspace.set_mcp_server_enabled("alpha", true, window, cx);
                workspace.set_mcp_server_enabled("beta", false, window, cx);
                assert!(workspace.mcp_status.is_busy(&directory, "alpha"));
                assert!(
                    workspace.mcp_status.is_busy(&directory, "beta"),
                    "another server's switch does not free or block this one"
                );
            });
        })
        .expect("test window");
        settle(window, cx);
        let disconnects = |name: &str| {
            let route = format!("/api/experimental/mcp/{name}/disconnect");
            fixture.count(|request| request.method == "POST" && request.target.starts_with(&route))
        };
        assert_eq!(disconnects("alpha"), 1);
        assert_eq!(disconnects("beta"), 1);
        assert_eq!(
            fixture.count(|request| request.method == "POST"),
            2,
            "no connect went out for the held server"
        );
        assert_eq!(
            fixture.count(|request| request.is("GET", "/api/config/mcp")),
            2,
            "one switch per server"
        );
        cx.read(|cx| {
            let status = &workspace.read(cx).mcp_status;
            assert!(!status.is_busy(&directory, "alpha"));
            assert!(!status.is_busy(&directory, "beta"));
        });
        assert_eq!(
            shown(&workspace, &directory, cx),
            [
                ("alpha".to_owned(), McpConnection::Disabled),
                ("beta".to_owned(), McpConnection::Disabled)
            ]
        );
    }
}
