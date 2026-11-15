//! Moving a session tree to a worktree or another project, and the worktree
//! a quick move creates.
//!
//! Follows `requestSessionTreeMove` (`packages/ui/src/lib/worktrees/
//! sessionWorktreeMove.ts`). Descendants move before the root, one request
//! at a time, and each step first checks that none of the remaining
//! sessions started a turn. A failed step moves the sessions that already
//! moved back in reverse. A worktree created for the move is removed after a
//! definite rejection, and kept when the answer was lost, because OpenCode
//! admits a move before it answers. Every request captures its runtime; a
//! completion for another runtime changes nothing here.

use std::collections::HashSet;
use std::time::Duration;

use gpui_kit::component::notification::Notification;
use gpui_kit::{Context, SharedString, Task, Window};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::ownership::{ProjectWorktrees, WorktreeInfo};
use openchamber_native_core::repository::GitStatusRead;
use openchamber_native_core::session::LiveStatus;
use openchamber_native_core::session_actions::{self, Submenu, SubmenuEntry};
use openchamber_native_core::session_moves;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::worktree::{Bootstrap, CreatedWorktree, NewWorktree};

use crate::locale::{self, Strings};
use crate::send_attempt::SessionKey;
use crate::session_management::describe_error;
use crate::workspace::Workspace;

/// How often a new worktree's setup is read while the move waits for it.
const BOOTSTRAP_POLL: Duration = Duration::from_millis(250);
/// How long a new worktree's checkout may take (`CHECKOUT_TIMEOUT_MS`).
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Moves in flight and the worktree list the open submenu reads.
#[derive(Default)]
pub(crate) struct WorktreeMoves {
    /// Roots whose tree is moving.
    pending: HashSet<SessionKey>,
    menu: Option<WorktreeMenu>,
}

impl WorktreeMoves {
    /// Forgets the submenu's read for a runtime switch. Moves in flight
    /// finish against their captured runtime.
    pub(crate) fn switch_runtime(&mut self) {
        self.menu = None;
    }
}

/// The worktree read one open submenu started.
struct WorktreeMenu {
    key: SessionKey,
    failed: bool,
    _read: Option<Task<()>>,
}

/// Where a tree goes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Destination {
    /// An existing worktree or the main checkout.
    Worktree(String),
    /// A worktree created for this move from the session's branch.
    NewWorktree,
    /// Another project's folder; the conversation moves, its files stay.
    Project(String),
}

/// The toast texts of one kind of move.
#[derive(Clone, Copy, Debug)]
struct Messages {
    success: &'static str,
    failure: &'static str,
}

/// A worktree this move created, removed again after a definite failure.
#[derive(Clone, Debug)]
struct Created {
    project: String,
    path: String,
}

/// One tree move between its steps.
struct MoveRun {
    root: SessionKey,
    /// Descendants first, the root last.
    order: Vec<SessionId>,
    moved: Vec<SessionId>,
    source: String,
    destination: String,
    created: Option<Created>,
    messages: Messages,
}

/// Why a move stopped.
#[derive(Clone, Debug)]
enum MoveFailure {
    /// A session of the tree runs a turn.
    Busy,
    /// The request failed with this explanation.
    Detail(SharedString),
    /// A request's answer was lost after a worktree was created; the
    /// worktree stays, since the move may still land there.
    OutcomeUnknown,
}

impl Workspace {
    /// Whether a move of `session`'s tree is in flight.
    pub(crate) fn worktree_move_pending(&self, session: &SessionId) -> bool {
        self.session_management
            .moves
            .pending
            .contains(&self.mutation_key(session))
    }

    /// The Move to worktree submenu for `session`: the known worktrees at
    /// once, then again when a fresh listing of its project answers.
    pub(crate) fn worktree_submenu_entries(
        &mut self,
        session: &SessionId,
        cx: &mut Context<Self>,
    ) -> Vec<SubmenuEntry> {
        let Some(record) = self.sessions.get(session) else {
            return Vec::new();
        };
        let source = record.resolved_directory();
        let project = source.as_deref().and_then(|source| {
            session_moves::project_for_directory(
                &self.topology.projects,
                &self.topology.worktrees,
                source,
            )
        });
        let project_path = project.map(|project| project.path.clone());
        let discovered: Vec<WorktreeInfo> = project_path
            .as_deref()
            .and_then(|path| {
                self.topology
                    .worktrees
                    .iter()
                    .find(|listing| listing.project_path == path)
            })
            .map(|listing| listing.worktrees.clone())
            .unwrap_or_default();
        let current = source.as_deref().and_then(|source| {
            discovered
                .iter()
                .filter(|worktree| session_moves::checkout_holds_directory(&worktree.path, source))
                .max_by_key(|worktree| worktree.path.len())
                .cloned()
        });
        let targets = session_moves::worktree_targets(
            project_path.as_deref(),
            &discovered,
            source.as_deref(),
            current.as_ref(),
        );
        let key = self.mutation_key(session);
        let failed = match &self.session_management.moves.menu {
            Some(menu) if menu.key == key => menu.failed,
            _ => {
                self.read_menu_worktrees(key, project_path, cx);
                false
            }
        };
        let busy = matches!(
            self.live_status.get(session),
            Some(LiveStatus::Busy | LiveStatus::Retry)
        );
        let enabled = self.session_actions_available()
            && source.is_some()
            && !busy
            && !self.worktree_move_pending(session);
        session_moves::worktree_submenu(&targets, failed, enabled)
    }

    /// Lists the project's worktrees again for the open submenu. The answer
    /// replaces that project's topology; a failure keeps what was known.
    fn read_menu_worktrees(
        &mut self,
        key: SessionKey,
        project_path: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let read = project_path.and_then(|path| {
            let connection = self.connection.as_ref()?;
            let client = connection.client.clone()?;
            let paths = vec![path];
            Some(
                connection
                    .network
                    .run(async move { client.project_worktrees(&paths).await }),
            )
        });
        let Some(read) = read else {
            self.session_management.moves.menu = Some(WorktreeMenu {
                key,
                failed: true,
                _read: None,
            });
            return;
        };
        let runtime = key.runtime.clone();
        let session = key.session.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                let current = this
                    .session_management
                    .moves
                    .menu
                    .as_ref()
                    .is_some_and(|menu| menu.key.runtime == runtime && menu.key.session == session);
                if !current || this.runtime != runtime {
                    return;
                }
                let listings = match result {
                    Some(Ok(reads)) => reads,
                    _ => Vec::new(),
                };
                let mut failed = listings.is_empty();
                for read in listings {
                    let Some(worktrees) = read.worktrees else {
                        failed = true;
                        continue;
                    };
                    let entry = ProjectWorktrees {
                        project_path: read.project_path,
                        worktrees,
                    };
                    match this
                        .topology
                        .worktrees
                        .iter_mut()
                        .find(|held| held.project_path == entry.project_path)
                    {
                        Some(held) => *held = entry,
                        None => this.topology.worktrees.push(entry),
                    }
                }
                if let Some(menu) = this.session_management.moves.menu.as_mut() {
                    menu.failed = failed;
                }
                this.rebuild_sidebar();
                this.refresh_session_submenu(Submenu::Worktree, cx);
            });
        });
        self.session_management.moves.menu = Some(WorktreeMenu {
            key,
            failed: false,
            _read: Some(task),
        });
    }

    /// Moves `session`'s tree to the worktree at `path`.
    pub(crate) fn move_session_to_worktree(
        &mut self,
        session: &SessionId,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_tree_move(
            session,
            Destination::Worktree(path.to_owned()),
            Messages {
                success: "sessions.sidebar.session.moveToWorktree.existingSuccess",
                failure: "sessions.sidebar.session.moveToWorktree.existingFailed",
            },
            window,
            cx,
        );
    }

    /// Creates a worktree from `session`'s branch and moves its tree there.
    pub(crate) fn move_session_to_new_worktree(
        &mut self,
        session: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_tree_move(
            session,
            Destination::NewWorktree,
            Messages {
                success: "sessions.sidebar.session.moveToWorktree.success",
                failure: "sessions.sidebar.session.moveToWorktree.failed",
            },
            window,
            cx,
        );
    }

    /// Moves `session`'s tree to another project's folder, from the Move
    /// session dialog.
    pub(crate) fn move_session_to_project(
        &mut self,
        session: &SessionId,
        directory: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_tree_move(
            session,
            Destination::Project(directory.to_owned()),
            Messages {
                success: "sessions.moveSession.success",
                failure: "sessions.moveSession.failed",
            },
            window,
            cx,
        );
    }

    /// Whether any session of `ids` runs a turn now.
    fn any_busy(&self, ids: &[SessionId]) -> bool {
        ids.iter().any(|id| {
            matches!(
                self.live_status.get(id),
                Some(LiveStatus::Busy | LiveStatus::Retry)
            )
        })
    }

    fn start_tree_move(
        &mut self,
        session: &SessionId,
        destination: Destination,
        messages: Messages,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = self.mutation_key(session);
        if self.session_management.moves.pending.contains(&root) {
            return;
        }
        let Some(source) = self
            .sessions
            .get(session)
            .and_then(|record| record.resolved_directory())
        else {
            return;
        };
        let targets = session_actions::archive_targets(&self.sessions, session);
        let order = session_moves::move_order(session, &targets);
        let mut run = MoveRun {
            root: root.clone(),
            order,
            moved: Vec::new(),
            source: source.clone(),
            destination: String::new(),
            created: None,
            messages,
        };
        self.session_management.moves.pending.insert(root);
        cx.notify();
        if self.any_busy(&run.order) {
            self.fail_tree_move(run, MoveFailure::Busy, window, cx);
            return;
        }
        match destination {
            Destination::Worktree(path) => {
                if session_moves::checkout_holds_directory(&path, &source) {
                    self.fail_tree_move(
                        run,
                        MoveFailure::Detail("Source and destination are the same".into()),
                        window,
                        cx,
                    );
                    return;
                }
                run.destination = path;
                self.step_tree_move(run, window, cx);
            }
            Destination::Project(directory) => {
                if openchamber_native_core::path::normalize(&directory)
                    == openchamber_native_core::path::normalize(&source)
                {
                    self.fail_tree_move(
                        run,
                        MoveFailure::Detail("Source and destination are the same".into()),
                        window,
                        cx,
                    );
                    return;
                }
                run.destination = directory;
                self.step_tree_move(run, window, cx);
            }
            Destination::NewWorktree => self.create_move_worktree(run, window, cx),
        }
    }

    /// Creates the worktree a quick move goes to, from the source's branch,
    /// and waits until Git has checked it out.
    fn create_move_worktree(&mut self, run: MoveRun, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = session_moves::project_for_directory(
            &self.topology.projects,
            &self.topology.worktrees,
            &run.source,
        )
        .map(|project| project.path.clone()) else {
            self.fail_tree_move(
                run,
                MoveFailure::Detail("Unable to find the project for this session".into()),
                window,
                cx,
            );
            return;
        };
        let source = run.source.clone();
        let name = new_branch_name();
        let runtime = run.root.runtime.clone();
        let root = run.root.clone();
        let started = self.spawn_session_task(
            move |client| Box::pin(prepare_quick_worktree(client, project, source, name)),
            move |this, result, window, cx| {
                let result = result.unwrap_or(Err(PrepareError::Cancelled));
                if this.runtime != runtime {
                    this.session_management.moves.pending.remove(&run.root);
                    return;
                }
                match result {
                    Ok((project, created)) => {
                        let mut run = run;
                        run.destination = created.path.clone();
                        run.created = Some(Created {
                            project,
                            path: created.path,
                        });
                        this.load_topology(window, cx);
                        this.step_tree_move(run, window, cx);
                    }
                    Err(error) => {
                        let strings = locale::strings(cx).clone();
                        let failure = MoveFailure::Detail(error.describe(&strings));
                        this.fail_tree_move(run, failure, window, cx);
                    }
                }
            },
            window,
            cx,
        );
        if !started {
            // Without a server the pending mark has nothing to wait for.
            self.session_management.moves.pending.remove(&root);
        }
    }

    /// Moves the next session of the run, after checking that none of the
    /// remaining sessions started a turn.
    fn step_tree_move(&mut self, mut run: MoveRun, window: &mut Window, cx: &mut Context<Self>) {
        let index = run.moved.len();
        let Some(next) = run.order.get(index).cloned() else {
            self.finish_tree_move(run, window, cx);
            return;
        };
        if self.any_busy(&run.order[index..]) {
            self.fail_tree_move(run, MoveFailure::Busy, window, cx);
            return;
        }
        let destination = run.destination.clone();
        let id = next.clone();
        let runtime = run.root.runtime.clone();
        let root = run.root.clone();
        let started = self.spawn_session_task(
            move |client| Box::pin(async move { client.move_session(&id, &destination).await }),
            move |this, result, window, cx| {
                if this.runtime != runtime {
                    this.session_management.moves.pending.remove(&run.root);
                    return;
                }
                match result {
                    Some(Ok(())) => {
                        run.moved.push(next);
                        this.step_tree_move(run, window, cx);
                    }
                    Some(Err(error)) => {
                        let unknown =
                            openchamber_native_transport::worktree::outcome_unknown(&error);
                        this.rollback_tree_move(run, error, unknown, window, cx);
                    }
                    None => {
                        this.session_management.moves.pending.remove(&run.root);
                    }
                }
            },
            window,
            cx,
        );
        if !started {
            self.session_management.moves.pending.remove(&root);
            cx.notify();
        }
    }

    /// Moves the sessions that already moved back, newest first. A busy one
    /// stays where it is and counts as a rollback failure.
    fn rollback_tree_move(
        &mut self,
        run: MoveRun,
        error: TransportError,
        unknown: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let strings = locale::strings(cx).clone();
        let detail = describe_error(&strings, &error);
        let back: Vec<SessionId> = run.moved.iter().rev().cloned().collect();
        let busy: Vec<SessionId> = back
            .iter()
            .filter(|id| self.any_busy(std::slice::from_ref(*id)))
            .cloned()
            .collect();
        let movable: Vec<SessionId> = back.into_iter().filter(|id| !busy.contains(id)).collect();
        let source = run.source.clone();
        let runtime = run.root.runtime.clone();
        let root = run.root.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    let mut failed = Vec::new();
                    for id in movable {
                        if let Err(error) = client.move_session(&id, &source).await {
                            failed.push((id, error));
                        }
                    }
                    failed
                })
            },
            move |this, result, window, cx| {
                if this.runtime != runtime {
                    this.session_management.moves.pending.remove(&run.root);
                    return;
                }
                let failures = result.map_or(busy.len() + 1, |failed| failed.len() + busy.len());
                if unknown {
                    // The session may already be in the destination: read
                    // both places now instead of waiting for the next event.
                    this.load_topology(window, cx);
                }
                if failures > 0 {
                    // The reference's own wording (`createIncompleteRollbackError`).
                    let text = format!(
                        "Session move partially failed and could not be fully rolled back: \
                         {detail}. Rollback failures: {failures}"
                    );
                    this.fail_tree_move(run, MoveFailure::Detail(text.into()), window, cx);
                    return;
                }
                if unknown && run.created.is_some() {
                    this.fail_tree_move(run, MoveFailure::OutcomeUnknown, window, cx);
                    return;
                }
                this.fail_tree_move(run, MoveFailure::Detail(detail), window, cx);
            },
            window,
            cx,
        );
        if !started {
            self.session_management.moves.pending.remove(&root);
        }
    }

    /// Reports a failed move. A worktree this move created goes again after
    /// a definite failure, with its local branch, since nothing of the
    /// user's is in it yet.
    fn fail_tree_move(
        &mut self,
        mut run: MoveRun,
        failure: MoveFailure,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let (Some(created), MoveFailure::Busy | MoveFailure::Detail(_)) =
            (run.created.take(), &failure)
        {
            let runtime = run.root.runtime.clone();
            let root = run.root.clone();
            let messages = run.messages;
            let reported = failure.clone();
            let started = self.spawn_session_task(
                move |client| {
                    Box::pin(async move {
                        client
                            .remove_worktree(&created.project, &created.path, true)
                            .await
                    })
                },
                move |this, result, window, cx| {
                    if this.runtime == runtime {
                        this.load_topology(window, cx);
                    }
                    let failure = match result {
                        Some(Ok(())) => failure,
                        _ => {
                            let strings = locale::strings(cx).clone();
                            let detail = failure_detail(&strings, &failure);
                            // The reference's own wording (`removeFailedWorktree`).
                            MoveFailure::Detail(
                                format!(
                                    "Session move failed and the new worktree could not be \
                                     removed: {detail}"
                                )
                                .into(),
                            )
                        }
                    };
                    this.report_tree_move(&run.root, run.messages, failure, window, cx);
                },
                window,
                cx,
            );
            if !started {
                self.report_tree_move(&root, messages, reported, window, cx);
            }
            return;
        }
        self.report_tree_move(&run.root, run.messages, failure, window, cx);
    }

    fn report_tree_move(
        &mut self,
        root: &SessionKey,
        messages: Messages,
        failure: MoveFailure,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session_management.moves.pending.remove(root);
        let strings = locale::strings(cx).clone();
        let detail = failure_detail(&strings, &failure);
        self.push_error(messages.failure, Some(detail), window, cx);
        cx.notify();
    }

    /// Every session moved: the new worktree takes the session's title as
    /// its task name, and the topology is read again.
    fn finish_tree_move(&mut self, run: MoveRun, window: &mut Window, cx: &mut Context<Self>) {
        self.session_management.moves.pending.remove(&run.root);
        if let Some(created) = &run.created {
            let title = self
                .sessions
                .get(&run.root.session)
                .map(|record| record.title.trim().to_owned())
                .filter(|title| !title.is_empty());
            if let Some(title) = title {
                let directory = created.path.clone();
                let id = run.root.session.clone();
                self.spawn_session_task(
                    move |client| {
                        Box::pin(async move {
                            let by_user = client.session_titled_by_user(&id).await.unwrap_or(false);
                            client.set_worktree_title(&directory, &title, by_user).await
                        })
                    },
                    |_, result, _, _| {
                        if let Some(Err(error)) = result {
                            tracing::warn!(%error, "the task name did not reach the new worktree");
                        }
                    },
                    window,
                    cx,
                );
            }
        }
        self.load_topology(window, cx);
        let text = locale::strings(cx).get(run.messages.success);
        self.push_notice(Notification::success(text.clone()), text, window, cx);
    }
}

fn failure_detail(strings: &Strings, failure: &MoveFailure) -> SharedString {
    match failure {
        MoveFailure::Busy => strings.get("sessions.sidebar.session.moveToWorktree.tooltipBusy"),
        MoveFailure::Detail(detail) => detail.clone(),
        MoveFailure::OutcomeUnknown => {
            strings.get("sessions.sidebar.session.moveToWorktree.outcomeUnknown")
        }
    }
}

/// `generateBranchName`: a fresh adjective-noun pair.
fn new_branch_name() -> String {
    use std::hash::{BuildHasher, Hasher};

    let mut salt = 0u8;
    session_moves::generate_branch_slug(|bound| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        salt = salt.wrapping_add(1);
        hasher.write_u8(salt);
        (hasher.finish() % bound as u64) as usize
    })
}

/// Why a quick worktree could not be prepared.
#[derive(Debug)]
enum PrepareError {
    Transport(TransportError),
    /// The source checkout is on no branch.
    NoBranch,
    /// Git setup failed and the worktree was removed again.
    Setup(Option<String>),
    /// Git setup failed and the worktree could not be removed.
    SetupCleanup(Option<String>),
    Cancelled,
}

impl PrepareError {
    /// The toast's description. Like the reference, which shows these
    /// errors' own messages, the texts that are not server answers are its
    /// English wording.
    fn describe(&self, strings: &Strings) -> SharedString {
        let setup = |error: &Option<String>| {
            error
                .clone()
                .unwrap_or_else(|| "Worktree bootstrap failed".to_owned())
        };
        match self {
            Self::Transport(error) => describe_error(strings, error),
            Self::NoBranch => "Unable to determine the current branch".into(),
            Self::Setup(error) => setup(error).into(),
            Self::SetupCleanup(error) => format!(
                "Session move failed and the new worktree could not be removed: {}",
                setup(error)
            )
            .into(),
            Self::Cancelled => "Worktree creation cancelled".into(),
        }
    }
}

/// Creates the worktree of a quick move on the network: the source's branch
/// as its start, the project's trusted setup commands, then a wait until
/// Git has checked it out. A failed checkout removes it again.
async fn prepare_quick_worktree(
    client: ServerClient,
    project: String,
    source: String,
    name: String,
) -> Result<(String, CreatedWorktree), PrepareError> {
    let start_ref = match client.git_status(&source).await {
        Ok(GitStatusRead::Repository(status)) => match status.branch {
            Some(branch) => Some(branch),
            None => return Err(PrepareError::NoBranch),
        },
        Ok(GitStatusRead::NotRepository) => None,
        Err(error) => return Err(PrepareError::Transport(error)),
    };
    let setup = client.worktree_setup(&project).await;
    let start_command =
        session_moves::start_command(session_moves::worktree_setup_commands(&setup), &project);
    let created = client
        .create_worktree(
            &project,
            &NewWorktree {
                name,
                start_ref,
                start_command,
                generated: true,
            },
        )
        .await
        .map_err(PrepareError::Transport)?;
    let started = tokio::time::Instant::now();
    let setup_error = loop {
        match client.worktree_bootstrap(&created.path).await {
            Ok(Bootstrap::Ready | Bootstrap::Pending { git_ready: true }) => {
                return Ok((project, created));
            }
            Ok(Bootstrap::Failed(error)) => break error,
            Ok(Bootstrap::Pending { git_ready: false }) => {}
            Err(error) => break Some(error.to_string()),
        }
        if started.elapsed() >= BOOTSTRAP_TIMEOUT {
            break None;
        }
        tokio::time::sleep(BOOTSTRAP_POLL).await;
    };
    match client.remove_worktree(&project, &created.path, true).await {
        Ok(()) => Err(PrepareError::Setup(setup_error)),
        Err(_) => Err(PrepareError::SetupCleanup(setup_error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_are_adjective_noun_pairs() {
        let name = new_branch_name();
        let (adjective, noun) = name.split_once('-').expect("a pair");
        assert!(!adjective.is_empty() && !noun.is_empty());
    }

    #[test]
    fn a_busy_tree_explains_itself_with_the_menu_hint() {
        let strings = Strings::load("en");
        assert_eq!(
            failure_detail(&strings, &MoveFailure::Busy),
            strings.get("sessions.sidebar.session.moveToWorktree.tooltipBusy")
        );
    }
}
