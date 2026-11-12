//! The Git surface (`GitView.tsx`): the branch with its sync actions, who
//! commits, the staged and unstaged changes as `ChangesPanel` lays them out,
//! the commit section and the selected change's diff.
//!
//! The server's status is the only authority for what the rows show. Index
//! mutations run one at a time in the order asked (`IndexQueue`), and each is
//! followed by a status read, failed or not, so no row keeps a state the
//! server did not report. Only the newest status read applies. Discarding
//! one file asks for a confirmation in place. Revert all asks in a dialog
//! for the paths listed when it opened, discards each with scope `all`
//! outside the queue, then reads the status once and reports one summary.
//!
//! Fetch, pull and push are planned network operations: the binding read and
//! the core's request building pick the tracked remote, the server plans,
//! then executes while this view reads the same operation ID every second.
//! Cancel is sent for that ID; a push cancelled mid-transfer can end as
//! outcome unknown, which the view shows as such. A lost execute answer is
//! resolved by reading the ID again, never by planning another operation.
//!
//! The header's repository views menu opens the Update branch dialog, which
//! merges a local or fetched remote branch or rebases onto it
//! (`update_branch.rs`); conflicts open the conflict dialog and a merge or
//! rebase in progress shows its banner (`conflicts.rs`). Stash and hunk
//! staging are not ported.

use std::time::Duration;

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, AppContext, Context, Div, Entity, FontWeight, Hsla, InteractiveElement,
    IntoElement, ParentElement, Render, Role, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, TestSupportExt, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::git_changes::{
    ChangeKind, ChangeRow, ChangeScope, ChangesRead, GitChanges, IndexMutation, IndexQueue,
    LineStat, NetworkAction, NetworkOperation, NetworkRefusal, StatusLoads, accept_snapshot,
    network_request,
};
use openchamber_native_transport::git::{Branches, GitError};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, IconButton, glyph};
use crate::context_panel::{ROW_LINE, ROW_SIZE, XS_LINE, XS_SIZE};
use crate::diff_view::DiffPane;
use crate::elevation;
use crate::files_view::{Notice, PANEL_ROW, PanelScope, empty_state, status_line, text_button};
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::panel_controls::{
    HEADER_LINE, HEADER_SIZE, LABEL_LINE, LABEL_SIZE, META_LINE, META_SIZE, MICRO_LINE, MICRO_SIZE,
};
use crate::theme::Palette;

mod conflicts;
mod identity;
mod repositories;
mod update_branch;

pub(crate) const UPDATE_PICKER_CONTEXT: &str = "GitUpdatePicker";

const OPERATION_POLL: Duration = Duration::from_secs(1);
const DIFF_HEIGHT: f32 = 260.0;

/// The panel header's row with its bottom line (`h-10 border-b`).
const HEADER_ROW: f32 = 41.0;
/// `GitHeader`'s state row (`h-10`).
const STATE_ROW: f32 = 40.0;
/// The list's side padding (`px-4`).
const LIST_PADDING: f32 = 16.0;
/// The end padding of a list row (`pe-2`).
const ROW_END_PADDING: f32 = 8.0;
/// A group header: a 20 px line in `py-2`.
const GROUP_HEADER: f32 = 36.0;
/// A change row: a 20 px line in `py-1.5`.
pub(crate) const CHANGE_ROW: f32 = 32.0;
/// The stage, unstage and revert controls (`size-5`).
const SYMBOL_BUTTON: f32 = 20.0;
/// The revert-all row: a 28 px button in `py-2`.
const REVERT_ALL_ROW: f32 = 44.0;
/// The commit section's title row: a 20 px line in `pt-2 pb-1`.
const COMMIT_HEADER: f32 = 32.0;
/// `CommitInput`'s single-line height.
const COMMIT_FIELD: f32 = 38.0;
/// The file glyph of a change row.
pub(crate) const FILE_ICON: &str = "file-text";

/// A network operation this view started.
struct OperationRun {
    action: NetworkAction,
    remote: String,
    /// The server's latest snapshot; `None` while planning.
    operation: Option<NetworkOperation>,
    /// The last read failed, so the shown state may be stale.
    unavailable: bool,
    _execute: Task<()>,
    poll: Option<Task<()>>,
}

/// What Revert all would discard, as listed when its dialog opened.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RevertAllTarget {
    directory: String,
    paths: Vec<String>,
}

/// A running Revert all: its paths show as busy until it ends.
struct RevertAllRun {
    paths: Vec<String>,
    _task: Task<()>,
}

pub(crate) struct GitView {
    scope: PanelScope,
    status: StatusLoads,
    status_task: Option<Task<()>>,
    queue: IndexQueue,
    mutation_task: Option<Task<()>>,
    selected: Option<(String, ChangeScope)>,
    pane: DiffPane,
    confirm_discard: Option<String>,
    /// Revert all waits in its dialog for these paths.
    revert_all_confirm: Option<RevertAllTarget>,
    revert_all: Option<RevertAllRun>,
    staged_collapsed: bool,
    changes_collapsed: bool,
    commit_input: Entity<InputState>,
    commit_task: Option<Task<()>>,
    branches: Option<Branches>,
    branches_open: bool,
    branch_input: Entity<InputState>,
    rename_input: Option<Entity<InputState>>,
    branch_task: Option<Task<()>>,
    operation: Option<OperationRun>,
    notice: Option<Notice>,
    conflicts: conflicts::Conflicts,
    identities: identity::Identities,
    repositories: repositories::Repositories,
    update: update_branch::UpdateBranch,
    _subscriptions: Vec<Subscription>,
}

impl GitView {
    #[cfg(test)]
    pub(crate) fn start_test_operation(&mut self, cx: &mut Context<Self>) {
        self.operation = Some(OperationRun {
            action: NetworkAction::Push,
            remote: "origin".into(),
            operation: Some(NetworkOperation {
                id: "test-operation".into(),
                runtime_id: "test-runtime".into(),
                action: NetworkAction::Push,
                state: openchamber_native_core::git_changes::OperationState::Running,
                completed_steps: Vec::new(),
                error: None,
            }),
            unavailable: false,
            _execute: cx.spawn(async |_, _| {}),
            poll: Some(self.poll_operation(cx)),
        });
    }
    pub(crate) fn new(scope: PanelScope, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let strings = locale::strings(cx);
        let commit_placeholder = strings.get("gitView.commit.messagePlaceholder");
        let branch_placeholder = strings.get("gitView.branch.newBranchPlaceholder");
        let commit_input = cx.new(|cx| InputState::new(window, cx).placeholder(commit_placeholder));
        let branch_input = cx.new(|cx| InputState::new(window, cx).placeholder(branch_placeholder));
        let commit_subscription =
            cx.subscribe_in(&commit_input, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.commit(window, cx);
                }
            });
        let branch_subscription =
            cx.subscribe_in(&branch_input, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.create_branch(window, cx);
                }
            });
        let view = Self {
            scope,
            status: StatusLoads::default(),
            status_task: None,
            queue: IndexQueue::default(),
            mutation_task: None,
            selected: None,
            pane: DiffPane::new(cx),
            confirm_discard: None,
            revert_all_confirm: None,
            revert_all: None,
            staged_collapsed: false,
            changes_collapsed: false,
            commit_input,
            commit_task: None,
            branches: None,
            branches_open: false,
            branch_input,
            rename_input: None,
            branch_task: None,
            operation: None,
            notice: None,
            conflicts: conflicts::Conflicts::default(),
            identities: identity::Identities::default(),
            repositories: repositories::Repositories::default(),
            update: update_branch::UpdateBranch::default(),
            _subscriptions: vec![commit_subscription, branch_subscription],
        };
        let entity = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = entity.update(cx, |view, cx| {
                view.read_identities(cx);
                view.discover_repositories(cx);
                view.refresh(cx);
            });
        });
        view
    }

    #[cfg(test)]
    pub(crate) fn notice_text(&self) -> Option<String> {
        self.notice.as_ref().map(|notice| notice.text.to_string())
    }

    #[cfg(test)]
    pub(crate) fn operation_polling(&self) -> bool {
        self.operation
            .as_ref()
            .is_some_and(|run| run.poll.is_some())
    }

    fn changes(&self) -> Option<&GitChanges> {
        self.status.repository()
    }

    /// Reads the status; only the newest read applies.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.operation.as_ref().is_some_and(|run| {
            run.unavailable
                && run
                    .operation
                    .as_ref()
                    .is_some_and(|operation| operation.state.is_open())
        }) {
            let poll = self.poll_operation(cx);
            if let Some(run) = self.operation.as_mut() {
                run.poll = Some(poll);
            }
        }
        self.read_author(cx);
        let ticket = self.status.begin();
        let Some(client) = self.scope.client(cx) else {
            self.status.complete(ticket, Err(()));
            self.notice = Some(Notice::error(
                locale::strings(cx).get("gitView.operation.state.unavailable"),
                None,
            ));
            cx.notify();
            return;
        };
        let directory = self.git_directory();
        let task = self
            .scope
            .run(async move { client.git_changes(&directory).await });
        self.status_task = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let strings = locale::strings(cx);
                let failed = result.is_err();
                if !this.status.complete(ticket, result.map_err(|_| ())) {
                    return;
                }
                this.status_task = None;
                if failed && this.status.current().is_some() {
                    this.notice = Some(Notice::error(
                        strings.get("gitView.toast.refreshRepositoryFailed"),
                        None,
                    ));
                }
                this.reconcile_selection(cx);
                this.root_status_read(cx);
                cx.notify();
            });
        }));
    }

    /// Keeps the selected change while the status still lists it, reading
    /// its diff again; drops it otherwise.
    fn reconcile_selection(&mut self, cx: &mut Context<Self>) {
        let Some((path, scope)) = self.selected.clone() else {
            return;
        };
        let listed = self.changes().is_some_and(|changes| {
            let rows = match scope {
                ChangeScope::Staged => changes.staged(),
                ChangeScope::Unstaged => changes.unstaged(),
            };
            rows.iter().any(|row| row.path == path)
        });
        if listed {
            let scope_of = self.git_scope();
            self.pane.show(
                &scope_of,
                &path,
                scope,
                |view: &mut Self| &mut view.pane,
                cx,
            );
        } else {
            self.selected = None;
            self.pane.clear(cx);
        }
    }

    fn select(&mut self, row: &ChangeRow, cx: &mut Context<Self>) {
        let selection = (row.path.clone(), row.scope);
        if self.selected.as_ref() == Some(&selection) {
            return;
        }
        self.selected = Some(selection);
        let scope = self.git_scope();
        self.pane.show(
            &scope,
            &row.path,
            row.scope,
            |view: &mut Self| &mut view.pane,
            cx,
        );
        cx.notify();
    }

    /// Every changed path of both groups, once each (`allChangePaths`).
    fn revert_all_target(&self) -> RevertAllTarget {
        let mut paths: Vec<String> = Vec::new();
        if let Some(changes) = self.changes() {
            for row in changes.staged().into_iter().chain(changes.unstaged()) {
                if !paths.contains(&row.path) {
                    paths.push(row.path);
                }
            }
        }
        RevertAllTarget {
            directory: self.git_directory(),
            paths,
        }
    }

    fn reverting(&self, path: &str) -> bool {
        self.revert_all
            .as_ref()
            .is_some_and(|run| run.paths.iter().any(|candidate| candidate == path))
    }

    #[cfg(test)]
    pub(crate) fn revert_all_running(&self) -> bool {
        self.revert_all.is_some()
    }

    /// How many paths the open Revert all dialog names.
    #[cfg(test)]
    pub(crate) fn revert_all_asking(&self) -> Option<usize> {
        self.revert_all_confirm
            .as_ref()
            .map(|target| target.paths.len())
    }

    /// Opens the Revert all dialog for the paths listed now.
    pub(crate) fn request_revert_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.revert_all.is_some() || self.revert_all_confirm.is_some() {
            return;
        }
        let target = self.revert_all_target();
        if target.paths.is_empty() {
            return;
        }
        self.revert_all_confirm = Some(target);
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = view.upgrade() else {
                return dialog;
            };
            let Some(target) = this.read(cx).revert_all_confirm.clone() else {
                return dialog;
            };
            let palette = cx.global::<Palette>().clone();
            let strings = locale::strings(cx).clone();
            let direction = strings.direction();
            let count = target.paths.len().to_string();
            let description = strings.format(
                if target.paths.len() == 1 {
                    "gitView.changes.revertAllDescriptionSingle"
                } else {
                    "gitView.changes.revertAllDescriptionPlural"
                },
                &[("count", &count)],
            );
            let (on_close, on_cancel, on_confirm) = (view.clone(), view.clone(), view.clone());
            let button = |id: &'static str, label: SharedString, fill: Option<Hsla>, text: Hsla| {
                div()
                    .id(id)
                    .role(Role::Button)
                    .accessibility_id(id)
                    .aria_label(label.clone())
                    .h(px(28.0))
                    .px(px(10.0))
                    .rounded(px(CONTROL_RADIUS))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_color(text)
                    .when_some(fill, |this, fill| this.bg(fill))
                    .when(fill.is_none(), |this| {
                        this.border_1().border_color(palette.border)
                    })
                    .child(div().text_size(gpui_kit::rems(LABEL_SIZE)).child(label))
                    .test_support()
            };
            dialog
                .w(px(448.0))
                .close_button(false)
                .on_close(move |_, _, cx| {
                    let _ = on_close.update(cx, |this, cx| {
                        this.revert_all_confirm = None;
                        cx.notify();
                    });
                })
                .title(strings.get("gitView.changes.revertAllDialogTitle"))
                .child(
                    div()
                        .id("git-revert-all-description")
                        .text_size(gpui_kit::rems(LABEL_SIZE))
                        .text_color(palette.muted_foreground)
                        .child(description),
                )
                .footer(
                    div()
                        .w_full()
                        .flex_row_from(direction)
                        .items_center()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            button(
                                "git-revert-all-cancel",
                                strings.get("gitView.common.cancel"),
                                None,
                                palette.foreground,
                            )
                            .on_click(move |_, window, cx| {
                                let _ = on_cancel
                                    .update(cx, |this, cx| this.cancel_revert_all(window, cx));
                            }),
                        )
                        .child(
                            button(
                                "git-revert-all-confirm",
                                strings.get("gitView.changes.revertAll"),
                                Some(palette.status_error),
                                palette.status_error_foreground,
                            )
                            .on_click(move |_, window, cx| {
                                let _ = on_confirm
                                    .update(cx, |this, cx| this.confirm_revert_all(window, cx));
                            }),
                        ),
                )
        });
        cx.notify();
    }

    pub(crate) fn cancel_revert_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.revert_all_confirm = None;
        window.close_dialog(cx);
        cx.notify();
    }

    /// Reverts exactly what the dialog named. When the listed paths or the
    /// repository changed meanwhile, the dialog asks again for what is
    /// listed now and nothing is reverted.
    pub(crate) fn confirm_revert_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(confirmed) = self.revert_all_confirm.take() else {
            return;
        };
        let current = self.revert_all_target();
        if current != confirmed {
            if current.paths.is_empty() {
                window.close_dialog(cx);
            } else {
                self.revert_all_confirm = Some(current);
            }
            cx.notify();
            return;
        }
        window.close_dialog(cx);
        self.start_revert_all(confirmed.paths, cx);
    }

    /// Discards `paths` one after another, keeps every failure, then reads
    /// the status once and reports one summary (`handleRevertPaths`).
    fn start_revert_all(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        if self.revert_all.is_some() {
            return;
        }
        // A path an index mutation is changing is left to that mutation.
        let paths: Vec<String> = paths
            .into_iter()
            .filter(|path| !self.queue.touches(path))
            .collect();
        if paths.is_empty() {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            self.notice = Some(Notice::error(
                locale::strings(cx).get("gitView.operation.state.unavailable"),
                None,
            ));
            cx.notify();
            return;
        };
        let directory = self.git_directory();
        let batch = paths.clone();
        let task = self.scope.run(async move {
            let mut failed: Vec<Option<String>> = Vec::new();
            for path in &batch {
                if let Err(error) = client.discard_all_changes(&directory, path).await {
                    failed.push(error.message().map(str::to_owned));
                }
            }
            failed
        });
        let total = paths.len();
        let task = cx.spawn(async move |this, cx| {
            let failed = task.await;
            let _ = this.update(cx, |this, cx| {
                this.revert_all = None;
                if let Some(failed) = failed {
                    this.notice = Some(revert_all_notice(total, &failed, locale::strings(cx)));
                }
                // One read for the whole batch; the summary stays unless
                // that read fails.
                this.refresh(cx);
                cx.notify();
            });
        });
        self.revert_all = Some(RevertAllRun { paths, _task: task });
        cx.notify();
    }

    /// Queues an index mutation; it runs once those before it finished.
    fn enqueue(&mut self, mutation: IndexMutation, cx: &mut Context<Self>) {
        if let Some(mutation) = self.queue.push(mutation) {
            self.run_mutation(mutation, cx);
        }
        cx.notify();
    }

    fn run_mutation(&mut self, mutation: IndexMutation, cx: &mut Context<Self>) {
        let Some(client) = self.scope.client(cx) else {
            self.queue = IndexQueue::default();
            self.notice = Some(Notice::error(
                locale::strings(cx).get("gitView.operation.state.unavailable"),
                None,
            ));
            cx.notify();
            return;
        };
        let directory = self.git_directory();
        let applied = mutation.clone();
        let task = self.scope.run(async move {
            match &applied {
                IndexMutation::Stage(paths) => client.stage(&directory, paths).await,
                IndexMutation::Unstage(paths) => client.unstage(&directory, paths).await,
                IndexMutation::Discard(path) => client.discard(&directory, path).await,
            }
        });
        self.mutation_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let strings = locale::strings(cx);
                match result {
                    Some(Ok(())) => {
                        if let IndexMutation::Discard(path) = &mutation {
                            this.notice = Some(Notice::info(
                                strings.format("gitView.toast.revertedFile", &[("path", path)]),
                            ));
                        }
                    }
                    Some(Err(error)) => {
                        let key = match mutation {
                            IndexMutation::Stage(_) => "gitView.toast.stageFileFailed",
                            IndexMutation::Unstage(_) => "gitView.toast.unstageFileFailed",
                            IndexMutation::Discard(_) => "gitView.toast.revertFailed",
                        };
                        this.notice = Some(Notice::error(strings.get(key), error.message()));
                    }
                    None => {}
                }
                // Failed or not, the server's status decides what shows.
                this.refresh(cx);
                match this.queue.finished() {
                    Some(next) => this.run_mutation(next, cx),
                    None => this.mutation_task = None,
                }
                cx.notify();
            });
        }));
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.commit_task.is_some() {
            return;
        }
        let strings = locale::strings(cx);
        let message = self.commit_input.read(cx).value().trim().to_owned();
        if message.is_empty() {
            self.notice = Some(Notice::error(
                strings.get("gitView.toast.enterCommitMessage"),
                None,
            ));
            cx.notify();
            return;
        }
        if self
            .changes()
            .is_none_or(|changes| changes.staged().is_empty())
        {
            self.notice = Some(Notice::error(
                strings.get("gitView.toast.stageFileToCommit"),
                None,
            ));
            cx.notify();
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.git_directory();
        let task = self
            .scope
            .run(async move { client.commit(&directory, &message).await });
        self.commit_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.commit_task = None;
                let strings = locale::strings(cx);
                match result {
                    Some(Ok(_)) => {
                        this.notice =
                            Some(Notice::info(strings.get("gitView.toast.commitCreated")));
                        this.commit_input
                            .update(cx, |input, cx| input.set_value("", window, cx));
                    }
                    Some(Err(error)) => {
                        this.notice = Some(Notice::error(
                            strings.get("gitView.toast.createCommitFailed"),
                            error.message(),
                        ));
                    }
                    None => {}
                }
                this.refresh(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn toggle_branches(&mut self, cx: &mut Context<Self>) {
        self.branches_open = !self.branches_open;
        self.rename_input = None;
        if self.branches_open {
            self.read_branches(cx);
        }
        cx.notify();
    }

    fn read_branches(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.git_directory();
        let task = self
            .scope
            .run(async move { client.branches(&directory).await });
        self.branch_task = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.branch_task = None;
                match result {
                    Ok(branches) => {
                        this.branches = Some(branches);
                        this.branches_read_for_update();
                    }
                    Err(error) => {
                        let strings = locale::strings(cx);
                        this.notice = Some(Notice::error(
                            strings.get("branchPickerDialog.error.failedToLoad"),
                            error.message(),
                        ));
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Runs one branch change, then reads the branches and the status.
    fn branch_change(&mut self, change: BranchChange, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.git_directory();
        let run = change.clone();
        let task = self.scope.run(async move {
            match &run {
                BranchChange::Create(name) => {
                    client.create_branch(&directory, name).await?;
                    client.checkout_branch(&directory, name).await
                }
                BranchChange::Checkout(name) => client.checkout_branch(&directory, name).await,
                BranchChange::Rename { from, to } => {
                    client.rename_branch(&directory, from, to).await
                }
                BranchChange::Delete(name) => client.delete_branch(&directory, name).await,
            }
        });
        self.branch_task = Some(cx.spawn_in(window, async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.branch_task = None;
                let strings = locale::strings(cx);
                this.notice = Some(branch_notice(&change, result, strings));
                if matches!(change, BranchChange::Create(_)) {
                    this.branch_input
                        .update(cx, |input, cx| input.set_value("", window, cx));
                }
                this.rename_input = None;
                this.read_branches(cx);
                this.refresh(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn create_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.branch_input.read(cx).value().trim().to_owned();
        if name.is_empty() {
            return;
        }
        self.branch_change(BranchChange::Create(name), window, cx);
    }

    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(current) = self.changes().and_then(|changes| changes.branch.clone()) else {
            return;
        };
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx);
            input.set_value(current.clone(), window, cx);
            input
        });
        let subscription =
            cx.subscribe_in(&input, window, move |this, input, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let to = input.read(cx).value().trim().to_owned();
                    if !to.is_empty() && to != current {
                        this.branch_change(
                            BranchChange::Rename {
                                from: current.clone(),
                                to,
                            },
                            window,
                            cx,
                        );
                    }
                }
            });
        self._subscriptions.push(subscription);
        input.update(cx, |input, cx| input.focus(window, cx));
        self.rename_input = Some(input);
        cx.notify();
    }

    /// Plans and runs a fetch, pull or push for the tracked remote.
    fn run_network(&mut self, action: NetworkAction, cx: &mut Context<Self>) {
        if self.operation.as_ref().is_some_and(|run| {
            run.operation
                .as_ref()
                .is_none_or(|operation| operation.state.is_open())
        }) {
            return;
        }
        let Some(changes) = self.changes().cloned() else {
            return;
        };
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let directory = self.git_directory();
        let plan = self.scope.run(async move {
            let binding = client
                .repository_binding(&directory)
                .await
                .map_err(PlanFailure::Git)?;
            let request = network_request(action, &directory, &binding, &changes)
                .map_err(PlanFailure::Refused)?;
            let planned = client
                .plan_operation(&request)
                .await
                .map_err(PlanFailure::Git)?;
            Ok::<_, PlanFailure>((request.remote, planned))
        });
        let execute = cx.spawn(async move |this, cx| {
            let Some(planned) = plan.await else {
                let _ = this.update(cx, |this, cx| {
                    this.operation = None;
                    this.notice = Some(Notice::error(
                        locale::strings(cx).get("gitView.operation.state.unavailable"),
                        None,
                    ));
                    cx.notify();
                });
                return;
            };
            let (remote, planned) = match planned {
                Ok(planned) => planned,
                Err(failure) => {
                    let _ = this.update(cx, |this, cx| {
                        let strings = locale::strings(cx);
                        this.notice = Some(plan_failure_notice(action, &failure, strings));
                        this.operation = None;
                        cx.notify();
                    });
                    return;
                }
            };
            let id = planned.id.clone();
            let started = this.update(cx, |this, cx| {
                let poll = this.poll_operation(cx);
                if let Some(run) = this.operation.as_mut() {
                    run.remote = remote;
                    run.operation = Some(planned);
                    run.poll = Some(poll);
                }
                cx.notify();
            });
            if started.is_err() {
                return;
            }
            let Ok(Some((execute_client, network))) = this.update(cx, |this, cx| {
                this.scope
                    .client(cx)
                    .map(|client| (client, this.scope.network.clone()))
            }) else {
                let _ = this.update(cx, |this, cx| {
                    this.operation = None;
                    this.notice = Some(Notice::error(
                        locale::strings(cx).get("gitView.operation.state.unavailable"),
                        None,
                    ));
                    cx.notify();
                });
                return;
            };
            let execute_id = id.clone();
            let executed = crate::network::NetworkTask::spawn_on(&network, async move {
                match execute_client.execute_operation(&execute_id).await {
                    Ok(operation) => Ok(operation),
                    // The answer was lost; only the same ID is read again.
                    Err(_) => execute_client.read_operation(&execute_id).await,
                }
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                match executed {
                    Some(Ok(operation)) => this.accept_operation(operation, cx),
                    _ => {
                        if let Some(run) = this.operation.as_mut() {
                            run.unavailable = true;
                        }
                    }
                }
                cx.notify();
            });
        });
        self.operation = Some(OperationRun {
            action,
            remote: String::new(),
            operation: None,
            unavailable: false,
            _execute: execute,
            poll: None,
        });
        self.notice = None;
        cx.notify();
    }

    /// Reads the running operation every second until it finishes.
    fn poll_operation(&self, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let mut failures = 0;
            loop {
                cx.background_executor().timer(OPERATION_POLL).await;
                let Ok(Some(id)) = this.update(cx, |this, _| {
                    this.operation
                        .as_ref()
                        .and_then(|run| run.operation.as_ref())
                        .filter(|operation| operation.state.is_open())
                        .map(|operation| operation.id.clone())
                }) else {
                    return;
                };
                let source = this.update(cx, |this, cx| {
                    this.scope
                        .client(cx)
                        .map(|client| (client, this.scope.network.clone()))
                });
                let read = match source {
                    Ok(Some((client, network))) => {
                        crate::network::NetworkTask::spawn_on(&network, async move {
                            client.read_operation(&id).await
                        })
                        .await
                    }
                    _ => None,
                };
                let applied = this.update(cx, |this, cx| {
                    match read {
                        Some(Ok(operation)) => this.accept_operation(operation, cx),
                        _ => {
                            if let Some(run) = this.operation.as_mut() {
                                run.unavailable = true;
                            }
                            this.notice = Some(Notice::error(
                                locale::strings(cx).get("gitView.operation.state.unavailable"),
                                None,
                            ));
                        }
                    }
                    cx.notify();
                    this.operation.as_ref().is_some_and(|run| !run.unavailable)
                });
                if matches!(applied, Ok(true)) {
                    failures = 0;
                } else {
                    failures += 1;
                }
                if failures >= 3 {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(run) = this.operation.as_mut() {
                            run.poll = None;
                        }
                        cx.notify();
                    });
                    return;
                }
                if applied.is_err() {
                    return;
                }
            }
        })
    }

    /// Applies a newer snapshot; a finished operation reads the status.
    fn accept_operation(&mut self, next: NetworkOperation, cx: &mut Context<Self>) {
        let Some(run) = self.operation.as_mut() else {
            return;
        };
        let Some(held) = run.operation.as_ref() else {
            return;
        };
        let was_open = held.state.is_open();
        match accept_snapshot(held, next) {
            Ok(operation) => {
                run.unavailable = false;
                let finished = was_open && !operation.state.is_open();
                run.operation = Some(operation);
                if finished {
                    run.poll = None;
                    let strings = locale::strings(cx);
                    self.notice = Some(operation_notice(run, strings));
                    self.refresh(cx);
                }
            }
            Err(_) => run.unavailable = true,
        }
    }

    pub(crate) fn cancel_operation(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .operation
            .as_ref()
            .and_then(|run| run.operation.as_ref())
            .filter(|operation| operation.state.is_open())
            .map(|operation| operation.id.clone())
        else {
            return;
        };
        let Some(client) = self.scope.client(cx) else {
            let strings = locale::strings(cx);
            self.notice = Some(Notice::error(
                strings.format(
                    "gitView.toast.syncActionFailed",
                    &[("action", &strings.get("gitView.operation.cancel"))],
                ),
                None,
            ));
            cx.notify();
            return;
        };
        let task = self
            .scope
            .run(async move { client.cancel_operation(&id).await });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Some(Ok(operation)) => this.accept_operation(operation, cx),
                    other => {
                        let detail = other
                            .as_ref()
                            .and_then(|result| result.as_ref().err())
                            .and_then(GitError::message);
                        let strings = locale::strings(cx);
                        this.notice = Some(Notice::error(
                            strings.format(
                                "gitView.toast.syncActionFailed",
                                &[("action", &strings.get("gitView.operation.cancel"))],
                            ),
                            detail,
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[derive(Clone, Debug)]
enum BranchChange {
    Create(String),
    Checkout(String),
    Rename { from: String, to: String },
    Delete(String),
}

enum PlanFailure {
    Refused(NetworkRefusal),
    Git(GitError),
}

fn branch_notice(change: &BranchChange, result: Result<(), GitError>, strings: &Strings) -> Notice {
    match (change, result) {
        (BranchChange::Create(name), Ok(())) => {
            Notice::info(strings.format("gitView.toast.createdBranch", &[("name", name)]))
        }
        (BranchChange::Checkout(name), Ok(())) => {
            Notice::info(strings.format("gitView.toast.checkedOut", &[("name", name)]))
        }
        (BranchChange::Rename { from, to }, Ok(())) => Notice::info(strings.format(
            "gitView.toast.renamedBranch",
            &[("oldName", from), ("newName", to)],
        )),
        (BranchChange::Delete(_), Ok(())) => {
            Notice::info(strings.get("branchPickerDialog.toast.branchDeleted"))
        }
        (BranchChange::Create(_), Err(error)) => Notice::error(
            strings.get("gitView.toast.createBranchFailed"),
            error.message(),
        ),
        (BranchChange::Checkout(name), Err(error)) => Notice::error(
            strings.format("gitView.toast.checkoutFailed", &[("name", name)]),
            error.message(),
        ),
        (BranchChange::Rename { from, to }, Err(error)) => Notice::error(
            strings.format(
                "gitView.toast.renameBranchFailed",
                &[("oldName", from), ("newName", to)],
            ),
            error.message(),
        ),
        (BranchChange::Delete(_), Err(error)) => Notice::error(
            strings.get("branchPickerDialog.toast.failedToDeleteBranch"),
            error.message(),
        ),
    }
}

fn action_label(action: NetworkAction, strings: &Strings) -> SharedString {
    strings.get(match action {
        NetworkAction::Fetch => "gitView.sync.fetch",
        NetworkAction::Pull => "gitView.sync.pull",
        NetworkAction::Push => "gitView.sync.push",
    })
}

fn plan_failure_notice(action: NetworkAction, failure: &PlanFailure, strings: &Strings) -> Notice {
    let title = strings.format(
        "gitView.toast.syncActionFailed",
        &[("action", &action_label(action, strings))],
    );
    match failure {
        PlanFailure::Git(error) => Notice::error(title, error.message()),
        PlanFailure::Refused(refusal) => {
            let reason = strings.get(match refusal {
                NetworkRefusal::NoRemote | NetworkRefusal::RemoteMissing => {
                    "gitView.sync.noRemoteTooltip"
                }
                NetworkRefusal::BranchRequired => "gitView.branch.detachedHead",
                NetworkRefusal::TrackingRequired => "gitView.pr.unpushed.unpublished",
                NetworkRefusal::NeedsAttention => "gitView.operation.recovery.repository",
                NetworkRefusal::AnonymousReadOnly => "gitView.toast.pushFailed",
            });
            Notice::error(title, Some(&reason))
        }
    }
}

fn operation_notice(run: &OperationRun, strings: &Strings) -> Notice {
    let Some(operation) = run.operation.as_ref() else {
        return Notice::info(strings.get("gitView.operation.state.unavailable"));
    };
    use openchamber_native_core::git_changes::OperationState;
    let state = strings.get(&format!(
        "gitView.operation.state.{}",
        operation.state.as_str()
    ));
    let title: SharedString = format!("{} · {state}", action_label(run.action, strings)).into();
    if operation.state == OperationState::Succeeded {
        // `gitView.toast.pushedToUpstream` loses its remote in zh-TW, so a
        // push reports its state as the other outcomes do.
        let key = match run.action {
            NetworkAction::Fetch => "gitView.toast.fetchedFromRemote",
            NetworkAction::Pull => "gitView.toast.pulledFromRemote",
            NetworkAction::Push => return Notice::info(title),
        };
        return Notice::info(strings.format(key, &[("name", &run.remote)]));
    }
    let detail = match operation.state {
        OperationState::OutcomeUnknown => {
            Some(strings.get("gitView.operation.unknownHint").to_string())
        }
        _ => operation.error.as_ref().map(|error| error.message.clone()),
    };
    Notice::error(title, detail.as_deref())
}

impl GitView {
    fn render_header(
        &self,
        changes: Option<&GitChanges>,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let branch: SharedString = changes
            .and_then(|changes| changes.branch.clone())
            .map(SharedString::from)
            .unwrap_or_else(|| strings.get("gitView.branch.detachedHead"));
        let hover = palette.hover;
        let branch_button = on_activate(
            div()
                .id("git-branch-button")
                .role(Role::Button)
                .accessibility_id("git-branch-button")
                .aria_label(branch.clone())
                .aria_expanded(self.branches_open)
                .h(px(28.0))
                .px(px(8.0))
                .min_w_0()
                .rounded(px(CONTROL_RADIUS))
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .child(glyph("git-branch", 16.0, palette.primary))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(palette.foreground)
                        .child(branch),
                )
                .child(glyph("arrow-down-s", 16.0, palette.muted_foreground)),
            cx,
            |this: &mut Self, _, cx| this.toggle_branches(cx),
        )
        .test_support();
        let counts = changes.map(|changes| (changes.ahead, changes.behind));
        let count = |text: String| {
            div()
                .css_text(XS_SIZE, XS_LINE, cx)
                .text_color(palette.muted_foreground)
                .child(SharedString::from(text))
        };
        div()
            .id("git-header")
            .h(px(HEADER_ROW))
            .flex_shrink_0()
            .px(px(6.0))
            .border_b_1()
            .border_color(palette.divider)
            .flex_row_from(direction)
            .items_center()
            .gap(px(4.0))
            .child(branch_button)
            .children(self.render_repository_button(palette, direction, cx))
            .child(div().flex_1())
            .children(self.render_repository_views_button(palette, cx))
            .child(text_button(
                "git-fetch",
                strings.get("gitView.sync.fetch"),
                None,
                palette,
                cx,
                |this: &mut Self, _, cx| this.run_network(NetworkAction::Fetch, cx),
            ))
            .child(text_button(
                "git-pull",
                strings.get("gitView.sync.pull"),
                None,
                palette,
                cx,
                |this: &mut Self, _, cx| this.run_network(NetworkAction::Pull, cx),
            ))
            .children(
                counts
                    .filter(|(_, behind)| *behind > 0)
                    .map(|(_, behind)| count(format!("↓{behind}"))),
            )
            .child(text_button(
                "git-push",
                strings.get("gitView.sync.push"),
                None,
                palette,
                cx,
                |this: &mut Self, _, cx| this.run_network(NetworkAction::Push, cx),
            ))
            .children(
                counts
                    .filter(|(ahead, _)| *ahead > 0)
                    .map(|(ahead, _)| count(format!("↑{ahead}"))),
            )
            .child(
                IconButton::new(
                    "git-refresh",
                    "refresh",
                    strings.get("gitView.history.refresh"),
                )
                .render(palette, cx, |this: &mut Self, _, cx| {
                    this.discover_repositories(cx);
                    this.refresh(cx)
                }),
            )
            .test_support()
            .into_any_element()
    }

    fn render_operation(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let run = self.operation.as_ref()?;
        let strings = locale::strings(cx);
        let state = match (&run.operation, run.unavailable) {
            (_, true) => strings.get("gitView.operation.state.unavailable"),
            (None, false) => strings.get("gitView.operation.state.planned"),
            (Some(operation), false) => strings.get(&format!(
                "gitView.operation.state.{}",
                operation.state.as_str()
            )),
        };
        let open = run
            .operation
            .as_ref()
            .is_none_or(|operation| operation.state.is_open());
        let label: SharedString = format!("{} · {state}", action_label(run.action, strings)).into();
        Some(
            div()
                .id("git-operation")
                .role(Role::Status)
                .accessibility_id("git-operation")
                .test_support()
                .aria_label(label.clone())
                .h(px(28.0))
                .px(px(12.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .css_text(XS_SIZE, XS_LINE, cx)
                        .text_color(palette.foreground)
                        .child(label),
                )
                .when(open && run.operation.is_some(), |this| {
                    this.child(text_button(
                        "git-operation-cancel",
                        strings.get("gitView.operation.cancel"),
                        None,
                        palette,
                        cx,
                        |this: &mut Self, _, cx| this.cancel_operation(cx),
                    ))
                })
                .into_any_element(),
        )
    }

    fn render_branches(
        &self,
        changes: Option<&GitChanges>,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.branches_open {
            return None;
        }
        let strings = locale::strings(cx);
        let current = changes.and_then(|changes| changes.branch.clone());
        let field = |input: &Entity<InputState>, id: &'static str| {
            div()
                .id(id)
                .role(Role::Group)
                .accessibility_id(id)
                .aria_label(strings.get("gitView.branch.newBranchPlaceholder"))
                .h(px(28.0))
                .mx(px(8.0))
                .px(px(8.0))
                .rounded(px(CONTROL_RADIUS))
                .bg(palette.background)
                .flex()
                .items_center()
                .child(
                    Input::new(input)
                        .appearance(false)
                        .bordered(false)
                        .text_size(gpui_kit::rems(ROW_SIZE)),
                )
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        for name in self
            .branches
            .iter()
            .flat_map(|branches| branches.local.iter())
        {
            let is_current = current.as_deref() == Some(name.as_str());
            if is_current && let Some(input) = self.rename_input.as_ref() {
                rows.push(field(input, "git-branch-rename-field").into_any_element());
                continue;
            }
            let hover = palette.hover;
            let checkout_name = name.clone();
            let row = on_activate(
                div()
                    .id(gpui_kit::ElementId::Name(
                        format!("git-branch:{name}").into(),
                    ))
                    .role(Role::ListItem)
                    .accessibility_id(SharedString::from(format!("git-branch:{name}")))
                    .aria_label(SharedString::from(name.clone()))
                    .aria_selected(is_current)
                    .h(px(PANEL_ROW))
                    .mx(px(4.0))
                    .px(px(8.0))
                    .rounded(px(CONTROL_RADIUS))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .when(is_current, |this| this.bg(palette.selection))
                    .when(!is_current, |this| this.hover(move |style| style.bg(hover)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .css_text(ROW_SIZE, ROW_LINE, cx)
                            .text_color(palette.foreground)
                            .child(SharedString::from(name.clone())),
                    ),
                cx,
                move |this: &mut Self, window, cx| {
                    if !is_current {
                        this.branch_change(
                            BranchChange::Checkout(checkout_name.clone()),
                            window,
                            cx,
                        );
                    }
                },
            )
            .test_support();
            let delete_name = name.clone();
            let action = if is_current {
                IconButton::new(
                    "git-branch-rename",
                    "edit",
                    strings.get("gitView.branch.renameTitle"),
                )
                .render(palette, cx, |this: &mut Self, window, cx| {
                    this.start_rename(window, cx)
                })
            } else {
                IconButton::new(
                    format!("git-branch-delete:{name}"),
                    "delete-bin",
                    strings.get("branchPickerDialog.actions.deleteAria"),
                )
                .render(palette, cx, move |this: &mut Self, window, cx| {
                    this.branch_change(BranchChange::Delete(delete_name.clone()), window, cx)
                })
            };
            rows.push(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .padding_end(direction, px(8.0))
                    .child(div().flex_1().min_w_0().child(row))
                    .child(action)
                    .into_any_element(),
            );
        }
        Some(
            div()
                .id("git-branches")
                .role(Role::List)
                .accessibility_id("git-branches")
                .aria_label(strings.get("gitView.branch.localBranches"))
                .py(px(4.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(field(&self.branch_input, "git-branch-create"))
                .children(rows)
                .into_any_element(),
        )
    }
}

/// The one outcome of a Revert all of `total` paths: all reverted, none, or
/// how many of each, so a later success never hides a failure.
fn revert_all_notice(total: usize, failed: &[Option<String>], strings: &Strings) -> Notice {
    let failures = failed.len();
    if failures == 0 {
        let count = total.to_string();
        return Notice::info(strings.format(
            if total == 1 {
                "gitView.toast.revertedFilesSingle"
            } else {
                "gitView.toast.revertedFilesPlural"
            },
            &[("count", &count)],
        ));
    }
    if failures >= total {
        return Notice::error(
            strings.get("gitView.toast.revertFailed"),
            failed.iter().flatten().next().map(String::as_str),
        );
    }
    let succeeded = total - failures;
    let (success, failures) = (succeeded.to_string(), failures.to_string());
    Notice::error(
        strings.format(
            if succeeded == 1 {
                "gitView.toast.revertedSomeSingle"
            } else {
                "gitView.toast.revertedSomePlural"
            },
            &[("success", &success), ("failed", &failures)],
        ),
        None,
    )
}

/// The letter and color of a change, as `describeChange` gives them.
pub(crate) fn change_descriptor(kind: ChangeKind, palette: &Palette) -> (&'static str, Hsla) {
    match kind {
        ChangeKind::Untracked => ("?", palette.status_info),
        ChangeKind::Added => ("A", palette.status_success),
        ChangeKind::Deleted => ("D", palette.status_error),
        ChangeKind::Renamed => ("R", palette.status_info),
        ChangeKind::Copied => ("C", palette.status_info),
        ChangeKind::Conflicted => ("!", palette.status_error),
        ChangeKind::Modified | ChangeKind::TypeChanged => ("M", palette.status_warning),
    }
}

/// A changed path on one line (`ChangedPathLabel.tsx`): the folder muted and
/// truncated first, the file name whole. Paths read left to right in every
/// interface direction.
pub(crate) fn changed_path_label<V>(path: &str, palette: &Palette, cx: &Context<V>) -> Div {
    let label = div().flex_1().min_w_0().flex().flex_row().overflow_hidden();
    let Some((folder, name)) = path.rsplit_once('/') else {
        return label.child(
            div()
                .min_w_0()
                .truncate()
                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                .text_color(palette.foreground)
                .child(SharedString::from(path.to_owned())),
        );
    };
    label
        .child(
            div()
                .min_w_0()
                .truncate()
                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                .text_color(palette.muted_foreground)
                .child(SharedString::from(folder.to_owned())),
        )
        .child(
            div()
                .flex_shrink_0()
                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                .text_color(palette.muted_foreground)
                .child("/"),
        )
        .child(
            div()
                .flex_shrink_0()
                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                .text_color(palette.foreground)
                .child(SharedString::from(name.to_owned())),
        )
}

/// `+N/-N` in the status colors, as `ChangeRow` writes it.
fn change_stats<V>(stat: Option<LineStat>, palette: &Palette, cx: &Context<V>) -> Div {
    let (insertions, deletions) = stat
        .map(|stat| (stat.insertions, stat.deletions))
        .unwrap_or_default();
    let part = |text: String, color: Hsla| {
        div()
            .css_text(MICRO_SIZE, MICRO_LINE, cx)
            .text_color(color)
            .child(SharedString::from(text))
    };
    div()
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .child(part(format!("+{insertions}"), palette.status_success))
        .child(part("/".to_owned(), palette.muted_foreground).mx(px(2.0)))
        .child(part(format!("-{deletions}"), palette.status_error))
}

/// The 20 px stage or unstage control of a row or a group: `+` or `-`.
fn symbol_button<V: 'static>(
    id: SharedString,
    symbol: &'static str,
    label: SharedString,
    palette: &Palette,
    cx: &Context<V>,
    action: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> AnyElement {
    let foreground = palette.foreground;
    on_activate(
        div()
            .id(gpui_kit::ElementId::Name(id.clone()))
            .role(Role::Button)
            .accessibility_id(id)
            .aria_label(label)
            .size(px(SYMBOL_BUTTON))
            .flex_shrink_0()
            .rounded(px(4.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_color(palette.muted_foreground)
            .hover(move |style| style.text_color(foreground))
            .child(
                div()
                    .css_text(MICRO_SIZE, MICRO_LINE, cx)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(symbol),
            ),
        cx,
        action,
    )
    .test_support()
    .into_any_element()
}

impl GitView {
    fn is_collapsed(&self, scope: ChangeScope) -> bool {
        match scope {
            ChangeScope::Staged => self.staged_collapsed,
            ChangeScope::Unstaged => self.changes_collapsed,
        }
    }

    fn toggle_collapsed(&mut self, scope: ChangeScope, cx: &mut Context<Self>) {
        match scope {
            ChangeScope::Staged => self.staged_collapsed = !self.staged_collapsed,
            ChangeScope::Unstaged => self.changes_collapsed = !self.changes_collapsed,
        }
        cx.notify();
    }

    /// The row under the header (`GitHeader`'s `h-10 px-3`): who commits.
    fn render_state_row(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        div()
            .id("git-state-row")
            .h(px(STATE_ROW))
            .flex_shrink_0()
            .px(px(12.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .child(self.render_identity_button(palette, direction, cx))
            .test_support()
            .into_any_element()
    }

    /// One group of `ChangesPanel`: its header, then its rows unless collapsed.
    fn render_section(
        &self,
        title: SharedString,
        rows: Vec<ChangeRow>,
        first: bool,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let Some(scope) = rows.first().map(|row| row.scope) else {
            return div().into_any_element();
        };
        let collapsed = self.is_collapsed(scope);
        let all: Vec<String> = rows.iter().map(|row| row.path.clone()).collect();
        let (scope_name, bulk_id, symbol, bulk_label) = match scope {
            ChangeScope::Staged => (
                "staged",
                "git-unstage-all",
                "-",
                strings.get("gitView.changes.unstageAllAria"),
            ),
            ChangeScope::Unstaged => (
                "unstaged",
                "git-stage-all",
                "+",
                strings.get("gitView.changes.stageAllAria"),
            ),
        };
        let bulk = symbol_button(
            bulk_id.into(),
            symbol,
            bulk_label,
            palette,
            cx,
            move |this: &mut Self, _, cx| {
                let mutation = match scope {
                    ChangeScope::Staged => IndexMutation::Unstage(all.clone()),
                    ChangeScope::Unstaged => IndexMutation::Stage(all.clone()),
                };
                this.enqueue(mutation, cx)
            },
        );
        let group_id = SharedString::from(format!("git-group:{scope_name}"));
        let toggle = on_activate(
            div()
                .id(gpui_kit::ElementId::Name(group_id.clone()))
                .role(Role::Button)
                .accessibility_id(group_id)
                .aria_label(title.clone())
                .aria_expanded(!collapsed)
                .flex_1()
                .min_w_0()
                .h(px(SYMBOL_BUTTON))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .css_text(HEADER_SIZE, HEADER_LINE, cx)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.foreground)
                        .child(title),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .css_text(META_SIZE, META_LINE, cx)
                        .text_color(palette.muted_foreground)
                        .child(SharedString::from(rows.len().to_string())),
                )
                .child(crate::sidebar_view::chevron(collapsed, direction, palette)),
            cx,
            move |this: &mut Self, _, cx| this.toggle_collapsed(scope, cx),
        )
        .test_support();
        // A later group's top line stands above its 36 px.
        let heading = div()
            .h(px(if first {
                GROUP_HEADER
            } else {
                GROUP_HEADER + 1.0
            }))
            .flex_shrink_0()
            .padding_end(direction, px(ROW_END_PADDING))
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .when(!first, |this| {
                // `mt-1 border-t border-edge-muted` between groups.
                this.mt(px(4.0))
                    .border_t_1()
                    .border_color(palette.border.opacity(0.4))
            })
            .child(bulk)
            .child(toggle);
        div()
            .flex()
            .flex_col()
            .child(heading)
            .when(!collapsed, |this| {
                this.children(
                    rows.into_iter().enumerate().map(|(index, row)| {
                        self.render_change(row, index > 0, palette, direction, cx)
                    }),
                )
            })
            .into_any_element()
    }

    /// One `ChangeRow` in the flat view: the stage control, the status
    /// letter, the file, its path and line counts, then revert.
    fn render_change(
        &self,
        row: ChangeRow,
        divided: bool,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let scope_name = match row.scope {
            ChangeScope::Staged => "staged",
            ChangeScope::Unstaged => "unstaged",
        };
        let id = format!("git-change:{scope_name}:{}", row.path);
        // `before:border-t before:border-edge`, short of the end padding.
        let divider = divided.then(|| {
            div()
                .absolute()
                .top_0()
                .inset_start(direction, px(0.0))
                .inset_end(direction, px(ROW_END_PADDING))
                .h(px(1.0))
                .bg(palette.divider)
        });
        if row.scope == ChangeScope::Unstaged
            && self.confirm_discard.as_deref() == Some(row.path.as_str())
        {
            let path = row.path.clone();
            return div()
                .id(gpui_kit::ElementId::Name(
                    format!("git-discard-confirm-row:{}", row.path).into(),
                ))
                .role(Role::Group)
                .aria_label(strings.format(
                    "gitView.changes.revertFileDescription",
                    &[("path", &row.path)],
                ))
                .relative()
                .h(px(CHANGE_ROW))
                .padding_start(direction, px(8.0))
                .padding_end(direction, px(ROW_END_PADDING))
                .bg(palette.status_warning_background)
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .children(divider)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .css_text(META_SIZE, META_LINE, cx)
                        .text_color(palette.foreground)
                        .child(strings.format(
                            "gitView.changes.revertFileDescription",
                            &[("path", &row.path)],
                        )),
                )
                .child(text_button(
                    "git-discard-confirm",
                    strings.get("gitView.changes.revertFileConfirm"),
                    None,
                    palette,
                    cx,
                    move |this: &mut Self, _, cx| {
                        this.confirm_discard = None;
                        this.enqueue(IndexMutation::Discard(path.clone()), cx);
                    },
                ))
                .child(text_button(
                    "git-discard-cancel",
                    strings.get("gitView.common.cancel"),
                    None,
                    palette,
                    cx,
                    |this: &mut Self, _, cx| {
                        this.confirm_discard = None;
                        cx.notify();
                    },
                ))
                .into_any_element();
        }
        let selected = self.selected.as_ref() == Some(&(row.path.clone(), row.scope));
        let busy = self.queue.touches(&row.path) || self.reverting(&row.path);
        let (code, color) = change_descriptor(row.kind, palette);
        let select_row = row.clone();
        let label: SharedString = row.path.clone().into();
        let main = on_activate(
            div()
                .id(gpui_kit::ElementId::Name(id.clone().into()))
                .role(Role::ListItem)
                .accessibility_id(SharedString::from(id))
                .aria_label(label)
                .aria_selected(selected)
                .flex_1()
                .min_w_0()
                .h(px(CHANGE_ROW))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .child(
                    div()
                        .w(px(16.0))
                        .flex_shrink_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .css_text(MICRO_SIZE, MICRO_LINE, cx)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(color)
                                .child(code),
                        ),
                )
                .child(glyph(FILE_ICON, 14.0, palette.status_info))
                .child(changed_path_label(&row.path, palette, cx))
                .child(change_stats(row.stat, palette, cx)),
            cx,
            move |this: &mut Self, _, cx| this.select(&select_row, cx),
        )
        .test_support();
        let path = row.path.clone();
        // The slots keep their width while a mutation of the path runs.
        let slot = || {
            div()
                .size(px(SYMBOL_BUTTON))
                .flex_shrink_0()
                .into_any_element()
        };
        let stage = match (busy, row.scope) {
            (true, _) => slot(),
            (false, ChangeScope::Staged) => {
                let path = path.clone();
                symbol_button(
                    format!("git-unstage:{path}").into(),
                    "-",
                    strings.format("gitView.changes.unstageFileAria", &[("path", &path)]),
                    palette,
                    cx,
                    move |this: &mut Self, _, cx| {
                        this.enqueue(IndexMutation::Unstage(vec![path.clone()]), cx)
                    },
                )
            }
            (false, ChangeScope::Unstaged) => {
                let path = path.clone();
                symbol_button(
                    format!("git-stage:{path}").into(),
                    "+",
                    strings.format("gitView.changes.stageFileAria", &[("path", &path)]),
                    palette,
                    cx,
                    move |this: &mut Self, _, cx| {
                        this.enqueue(IndexMutation::Stage(vec![path.clone()]), cx)
                    },
                )
            }
        };
        let revert = match (busy, row.scope) {
            (_, ChangeScope::Staged) => None,
            (true, ChangeScope::Unstaged) => Some(slot()),
            (false, ChangeScope::Unstaged) => Some(
                IconButton::new(
                    format!("git-discard:{path}"),
                    "arrow-go-back",
                    strings.get("gitView.changes.revertFileTooltip"),
                )
                .size(SYMBOL_BUTTON, 14.0)
                .render(palette, cx, move |this: &mut Self, _, cx| {
                    this.confirm_discard = Some(path.clone());
                    cx.notify();
                }),
            ),
        };
        div()
            .relative()
            .h(px(CHANGE_ROW))
            .padding_end(direction, px(ROW_END_PADDING))
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .children(divider)
            .child(stage)
            .child(main)
            .children(revert)
            .into_any_element()
    }

    /// `ChangesPanel`'s last row: the button that asks to revert every
    /// listed path. It is disabled while such a batch runs.
    fn render_revert_all(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let label = strings.get("gitView.changes.revertAll");
        let running = self.revert_all.is_some();
        let tint = palette.status_error.opacity(0.1);
        // `Button variant="ghost" size="sm"` in the error color.
        let button = on_activate(
            div()
                .id("git-revert-all")
                .role(Role::Button)
                .accessibility_id("git-revert-all")
                .aria_label(label.clone())
                .h(px(28.0))
                .px(px(8.0))
                .flex_shrink_0()
                .rounded(px(CONTROL_RADIUS))
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .when(running, |this| this.opacity(0.5))
                .when(!running, |this| {
                    this.cursor_pointer().hover(move |style| style.bg(tint))
                })
                .child(glyph("arrow-go-back", 14.0, palette.status_error))
                .child(
                    div()
                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(palette.status_error)
                        .child(label),
                ),
            cx,
            |this: &mut Self, window, cx| this.request_revert_all(window, cx),
        )
        .test_support();
        div()
            .h(px(REVERT_ALL_ROW))
            .flex_shrink_0()
            .padding_end(direction, px(ROW_END_PADDING))
            .flex_row_from(direction)
            .items_center()
            .child(div().flex_1())
            .child(button)
            .into_any_element()
    }

    /// `CommitSection`: its title with the staging hint, the message field
    /// and the commit action.
    fn render_commit(
        &self,
        staged: usize,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let committing = self.commit_task.is_some();
        let label = if committing {
            strings.get("gitView.commit.committing")
        } else {
            strings.get("gitView.commit.commit")
        };
        let can_commit = staged > 0
            && !committing
            && !self.queue.is_busy()
            && !self.commit_input.read(cx).value().trim().is_empty();
        let hover = palette.hover;
        // `Button variant="outline" size="sm"` around the commit glyph.
        let commit = on_activate(
            div()
                .id("git-commit")
                .role(Role::Button)
                .accessibility_id("git-commit")
                .aria_label(label.clone())
                .h(px(28.0))
                .px(px(8.0))
                .flex_shrink_0()
                .rounded(px(CONTROL_RADIUS))
                .bg(palette.elevated)
                .shadow(elevation::control(palette))
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .when(can_commit, |this| {
                    this.cursor_pointer().hover(move |style| style.bg(hover))
                })
                .when(!can_commit, |this| this.opacity(0.5))
                .child(glyph("git-commit", 16.0, palette.foreground))
                .child(
                    div()
                        .css_text(LABEL_SIZE, LABEL_LINE, cx)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(palette.foreground)
                        .child(label),
                ),
            cx,
            // The commit reports an empty message or an empty index itself.
            |this: &mut Self, window, cx| this.commit(window, cx),
        )
        .test_support();
        div()
            .id("git-commit-section")
            .flex_shrink_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(COMMIT_HEADER))
                    .pt(px(8.0))
                    .pb(px(4.0))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_shrink_0()
                            .css_text(HEADER_SIZE, HEADER_LINE, cx)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(palette.foreground)
                            .child(strings.get("gitView.commit.title")),
                    )
                    .when(staged == 0, |this| {
                        this.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .css_text(META_SIZE, META_LINE, cx)
                                .text_color(palette.muted_foreground)
                                .child(strings.get("gitView.commit.stageFilesHint")),
                        )
                    }),
            )
            .child(
                div()
                    .pt(px(4.0))
                    .pb(px(12.0))
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .id("git-commit-message")
                            .role(Role::Group)
                            .accessibility_id("git-commit-message")
                            .aria_label(strings.get("gitView.commit.messagePlaceholder"))
                            .h(px(COMMIT_FIELD))
                            .px(px(12.0))
                            .rounded(px(8.0))
                            .bg(palette.elevated)
                            .shadow(elevation::control(palette))
                            .flex()
                            .items_center()
                            .child(
                                Input::new(&self.commit_input)
                                    .appearance(false)
                                    .bordered(false)
                                    .text_size(gpui_kit::rems(LABEL_SIZE)),
                            )
                            .test_support(),
                    )
                    .child(
                        div()
                            .h(px(28.0))
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(8.0))
                            .child(div().flex_1())
                            .child(commit),
                    ),
            )
            .into_any_element()
    }
}

impl Render for GitView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for (input, key) in [
            (&self.commit_input, "gitView.commit.messagePlaceholder"),
            (&self.branch_input, "gitView.branch.newBranchPlaceholder"),
        ] {
            let placeholder = locale::strings(cx).get(key);
            if input.read(cx).presentation().placeholder() != &placeholder {
                input.update(cx, |input, cx| {
                    input.set_placeholder(placeholder, window, cx)
                });
            }
        }
        let palette = cx.global::<Palette>().clone();
        let strings = locale::strings(cx);
        let direction = strings.direction();
        let changes = self.changes().cloned();
        let body: AnyElement = match self.status.current() {
            None if self.status.failed() => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(empty_state(
                    strings.get("gitView.toast.refreshRepositoryFailed"),
                    &palette,
                    cx,
                ))
                .child(text_button(
                    "git-retry",
                    strings.get("gitView.empty.retryDiscovery"),
                    None,
                    &palette,
                    cx,
                    |this: &mut Self, _, cx| this.refresh(cx),
                ))
                .into_any_element(),
            None => empty_state(
                strings.get("gitView.loading.checkingRepository"),
                &palette,
                cx,
            ),
            Some(ChangesRead::NotRepository) => {
                empty_state(strings.get("gitView.empty.notGitRepository"), &palette, cx)
            }
            Some(ChangesRead::Repository(repository)) if repository.is_clean() => {
                empty_state(strings.get("gitView.empty.cleanTitle"), &palette, cx)
            }
            Some(ChangesRead::Repository(repository)) => {
                let staged = repository.staged();
                let unstaged = repository.unstaged();
                let staged_count = staged.len();
                let staged_first = !staged.is_empty();
                // `px-4 pt-1 pb-4`, the list over the commit section, `gap-3`.
                div()
                    .size_full()
                    .px(px(LIST_PADDING))
                    .pt(px(4.0))
                    .pb(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .id("git-changes")
                            .role(Role::List)
                            .accessibility_id("git-changes")
                            .aria_label(strings.get("gitView.changes.changedFilesAria"))
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .when(staged_first, |this| {
                                this.child(self.render_section(
                                    strings.get("gitView.changes.stagedTitle"),
                                    staged,
                                    true,
                                    &palette,
                                    direction,
                                    cx,
                                ))
                            })
                            .when(!unstaged.is_empty(), |this| {
                                this.child(self.render_section(
                                    strings.get("gitView.changes.title"),
                                    unstaged,
                                    !staged_first,
                                    &palette,
                                    direction,
                                    cx,
                                ))
                            })
                            // Shown whenever a path is listed; this arm has one.
                            .child(self.render_revert_all(&palette, direction, cx))
                            .test_support(),
                    )
                    .child(self.render_commit(staged_count, &palette, direction, cx))
                    .into_any_element()
            }
        };
        let repository = matches!(self.status.current(), Some(ChangesRead::Repository(_)));
        div()
            .id("git-panel")
            .role(Role::Group)
            .accessibility_id("git-panel")
            .aria_label(strings.get("layout.rightSidebar.git"))
            .size_full()
            .flex()
            .flex_col()
            .when(repository, |this| {
                this.child(self.render_header(changes.as_ref(), &palette, direction, cx))
                    .child(self.render_state_row(&palette, direction, cx))
            })
            .children(self.render_operation(&palette, direction, cx))
            .children(self.render_repositories(&palette, cx))
            .children(self.render_identities(&palette, cx))
            .children(self.render_repository_views(&palette, direction, cx))
            .children(self.render_in_progress(changes.as_ref(), &palette, direction, cx))
            // The reference reports outcomes in toasts; the line takes its
            // row only while there is an outcome to read.
            .when(self.notice.is_some(), |this| {
                this.child(status_line(
                    "git-notice",
                    self.notice.as_ref(),
                    &palette,
                    direction,
                    cx,
                ))
            })
            .children(self.render_branches(changes.as_ref(), &palette, direction, cx))
            .child(div().flex_1().min_h_0().child(body))
            .when(self.selected.is_some(), |this| {
                this.child(div().h(px(1.0)).bg(palette.divider)).child(
                    div()
                        .h(px(DIFF_HEIGHT))
                        .flex_shrink_0()
                        .child(self.pane.render(false, &palette, cx)),
                )
            })
            .relative()
            .children(self.render_update_branch(&palette, direction, cx))
            .children(self.render_conflict_dialog(&palette, direction, cx))
    }
}
