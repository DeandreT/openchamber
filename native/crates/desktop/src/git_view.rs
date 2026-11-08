//! The Git surface (`GitView.tsx`): the branch with its sync actions, the
//! staged and unstaged changes, the commit field and the selected change's
//! diff.
//!
//! The server's status is the only authority for what the rows show. Index
//! mutations run one at a time in the order asked (`IndexQueue`), and each is
//! followed by a status read, failed or not, so no row keeps a state the
//! server did not report. Only the newest status read applies. Discarding
//! asks for a confirmation in place.
//!
//! Fetch, pull and push are planned network operations: the binding read and
//! the core's request building pick the tracked remote, the server plans,
//! then executes while this view reads the same operation ID every second.
//! Cancel is sent for that ID; a push cancelled mid-transfer can end as
//! outcome unknown, which the view shows as such. A lost execute answer is
//! resolved by reading the ID again, never by planning another operation.
//!
//! A branch row merges that branch into the current one or rebases onto it;
//! conflicts open the conflict dialog and a merge or rebase in progress shows
//! its banner (`conflicts.rs`). Stash and hunk staging are not ported.

use std::time::Duration;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, Role, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Task, TestSupportExt, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::git_changes::{
    ChangeRow, ChangeScope, ChangesRead, GitChanges, IndexMutation, IndexQueue, Integration,
    NetworkAction, NetworkOperation, NetworkRefusal, StatusLoads, accept_snapshot, network_request,
};
use openchamber_native_transport::git::{Branches, GitError};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, IconButton, glyph};
use crate::context_panel::{ROW_LINE, ROW_SIZE, XS_LINE, XS_SIZE};
use crate::diff_view::DiffPane;
use crate::files_view::{Notice, PANEL_ROW, PanelScope, empty_state, status_line, text_button};
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;

mod conflicts;
mod identity;
mod repositories;

const OPERATION_POLL: Duration = Duration::from_secs(1);
const DIFF_HEIGHT: f32 = 260.0;

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

pub(crate) struct GitView {
    scope: PanelScope,
    status: StatusLoads,
    status_task: Option<Task<()>>,
    queue: IndexQueue,
    mutation_task: Option<Task<()>>,
    selected: Option<(String, ChangeScope)>,
    pane: DiffPane,
    confirm_discard: Option<String>,
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
                    Ok(branches) => this.branches = Some(branches),
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

fn kind_color(row: &ChangeRow, palette: &Palette) -> gpui_kit::Hsla {
    use openchamber_native_core::git_changes::ChangeKind;
    match row.kind {
        ChangeKind::Added | ChangeKind::Untracked => palette.status_success,
        ChangeKind::Deleted | ChangeKind::Conflicted => palette.status_error,
        _ => palette.status_warning,
    }
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
                .child(glyph("git-branch", 14.0, palette.muted_foreground))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .css_text(ROW_SIZE, ROW_LINE, cx)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(palette.foreground)
                        .child(branch),
                )
                .child(crate::sidebar_view::chevron(
                    !self.branches_open,
                    direction,
                    palette,
                )),
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
            .h(px(40.0))
            .px(px(8.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(4.0))
            .child(branch_button)
            .children(self.render_repository_button(palette, direction, cx))
            .child(div().flex_1())
            .child(self.render_identity_button(palette, direction, cx))
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
            let merge_name = name.clone();
            let rebase_name = name.clone();
            let integrations = (!is_current && current.is_some()).then(|| {
                div()
                    .flex_row_from(direction)
                    .child(
                        IconButton::new(
                            format!("git-branch-merge:{name}"),
                            "git-merge",
                            strings.get("gitView.operation.merge"),
                        )
                        .render(
                            palette,
                            cx,
                            move |this: &mut Self, window, cx| {
                                this.integrate(Integration::Merge, merge_name.clone(), window, cx)
                            },
                        ),
                    )
                    .child(
                        IconButton::new(
                            format!("git-branch-rebase:{name}"),
                            "git-branch",
                            strings.get("gitView.operation.rebase"),
                        )
                        .render(
                            palette,
                            cx,
                            move |this: &mut Self, window, cx| {
                                this.integrate(Integration::Rebase, rebase_name.clone(), window, cx)
                            },
                        ),
                    )
            });
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
                    .children(integrations)
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

    fn render_section(
        &self,
        title: SharedString,
        rows: Vec<ChangeRow>,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let scope = rows.first().map(|row| row.scope);
        let all: Vec<String> = rows.iter().map(|row| row.path.clone()).collect();
        let bulk = scope.map(|scope| match scope {
            ChangeScope::Staged => IconButton::new(
                "git-unstage-all",
                "subtract",
                strings.get("gitView.changes.unstageAllAria"),
            )
            .render(palette, cx, move |this: &mut Self, _, cx| {
                this.enqueue(IndexMutation::Unstage(all.clone()), cx)
            }),
            ChangeScope::Unstaged => IconButton::new(
                "git-stage-all",
                "add",
                strings.get("gitView.changes.stageAllAria"),
            )
            .render(palette, cx, move |this: &mut Self, _, cx| {
                this.enqueue(IndexMutation::Stage(all.clone()), cx)
            }),
        });
        let heading = div()
            .h(px(28.0))
            .px(px(12.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .css_text(XS_SIZE, XS_LINE, cx)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(palette.foreground)
                    .child(title),
            )
            .child(
                div()
                    .css_text(XS_SIZE, XS_LINE, cx)
                    .text_color(palette.muted_foreground)
                    .child(SharedString::from(rows.len().to_string())),
            )
            .child(div().flex_1())
            .children(bulk);
        div()
            .flex()
            .flex_col()
            .child(heading)
            .children(
                rows.into_iter()
                    .map(|row| self.render_change(row, palette, direction, cx)),
            )
            .into_any_element()
    }

    fn render_change(
        &self,
        row: ChangeRow,
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
                .min_h(px(PANEL_ROW))
                .px(px(12.0))
                .py(px(2.0))
                .bg(palette.status_warning_background)
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .css_text(XS_SIZE, XS_LINE, cx)
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
        let busy = self.queue.touches(&row.path);
        let hover = palette.hover;
        let stat = row
            .stat
            .map(|stat| format!("+{} −{}", stat.insertions, stat.deletions));
        let select_row = row.clone();
        let label: SharedString = row.path.clone().into();
        let main = on_activate(
            div()
                .id(gpui_kit::ElementId::Name(id.clone().into()))
                .role(Role::ListItem)
                .accessibility_id(SharedString::from(id))
                .aria_label(label.clone())
                .aria_selected(selected)
                .flex_1()
                .min_w_0()
                .h(px(PANEL_ROW))
                .px(px(8.0))
                .rounded(px(CONTROL_RADIUS))
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .when(selected, |this| this.bg(palette.selection))
                .when(!selected, |this| this.hover(move |style| style.bg(hover)))
                .child(
                    div()
                        .w(px(12.0))
                        .flex_shrink_0()
                        .css_text(XS_SIZE, XS_LINE, cx)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(kind_color(&row, palette))
                        .child(SharedString::from(row.kind.letter().to_string())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .css_text(ROW_SIZE, ROW_LINE, cx)
                        .text_color(palette.foreground)
                        .child(label),
                )
                .children(stat.map(|stat| {
                    div()
                        .flex_shrink_0()
                        .css_text(XS_SIZE, XS_LINE, cx)
                        .text_color(palette.muted_foreground)
                        .child(SharedString::from(stat))
                })),
            cx,
            move |this: &mut Self, _, cx| this.select(&select_row, cx),
        )
        .test_support();
        let path = row.path.clone();
        let actions: Vec<AnyElement> = if busy {
            Vec::new()
        } else {
            match row.scope {
                ChangeScope::Staged => vec![
                    IconButton::new(
                        format!("git-unstage:{path}"),
                        "subtract",
                        strings.format("gitView.changes.unstageFileAria", &[("path", &path)]),
                    )
                    .render(palette, cx, move |this: &mut Self, _, cx| {
                        this.enqueue(IndexMutation::Unstage(vec![path.clone()]), cx)
                    }),
                ],
                ChangeScope::Unstaged => {
                    let discard = path.clone();
                    vec![
                        IconButton::new(
                            format!("git-discard:{path}"),
                            "arrow-go-back",
                            strings.get("gitView.changes.revertFileTooltip"),
                        )
                        .render(
                            palette,
                            cx,
                            move |this: &mut Self, _, cx| {
                                this.confirm_discard = Some(discard.clone());
                                cx.notify();
                            },
                        ),
                        IconButton::new(
                            format!("git-stage:{path}"),
                            "add",
                            strings.format("gitView.changes.stageFileAria", &[("path", &path)]),
                        )
                        .render(
                            palette,
                            cx,
                            move |this: &mut Self, _, cx| {
                                this.enqueue(IndexMutation::Stage(vec![path.clone()]), cx)
                            },
                        ),
                    ]
                }
            }
        };
        div()
            .px(px(4.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(2.0))
            .child(main)
            // A fixed slot, so a row keeps its width while its actions run.
            .child(
                div()
                    .w(px(52.0))
                    .flex_shrink_0()
                    .flex_row_from(direction)
                    .justify_end()
                    .children(actions),
            )
            .into_any_element()
    }

    fn render_commit(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let label = if self.commit_task.is_some() {
            strings.get("gitView.commit.committing")
        } else {
            strings.get("gitView.commit.commit")
        };
        div()
            .px(px(8.0))
            .py(px(6.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .id("git-commit-message")
                    .role(Role::Group)
                    .accessibility_id("git-commit-message")
                    .aria_label(strings.get("gitView.commit.messagePlaceholder"))
                    .flex_1()
                    .min_w_0()
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(CONTROL_RADIUS))
                    .bg(palette.background)
                    .flex()
                    .items_center()
                    .child(
                        Input::new(&self.commit_input)
                            .appearance(false)
                            .bordered(false)
                            .text_size(gpui_kit::rems(ROW_SIZE)),
                    ),
            )
            .child(text_button(
                "git-commit",
                label,
                None,
                palette,
                cx,
                |this: &mut Self, window, cx| this.commit(window, cx),
            ))
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
                div()
                    .id("git-changes")
                    .role(Role::Group)
                    .accessibility_id("git-changes")
                    .aria_label(strings.get("gitView.changes.changedFilesAria"))
                    .size_full()
                    .overflow_y_scroll()
                    .py(px(4.0))
                    .when(!staged.is_empty(), |this| {
                        this.child(self.render_section(
                            strings.get("gitView.changes.stagedTitle"),
                            staged,
                            &palette,
                            direction,
                            cx,
                        ))
                    })
                    .when(!unstaged.is_empty(), |this| {
                        this.child(self.render_section(
                            strings.get("gitView.changes.title"),
                            unstaged,
                            &palette,
                            direction,
                            cx,
                        ))
                    })
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
            })
            .children(self.render_operation(&palette, direction, cx))
            .children(self.render_repositories(&palette, cx))
            .children(self.render_identities(&palette, cx))
            .children(self.render_in_progress(changes.as_ref(), &palette, direction, cx))
            .child(status_line(
                "git-notice",
                self.notice.as_ref(),
                &palette,
                direction,
                cx,
            ))
            .children(self.render_branches(changes.as_ref(), &palette, direction, cx))
            .child(div().flex_1().min_h_0().child(body))
            .when(repository, |this| {
                this.child(self.render_commit(&palette, direction, cx))
            })
            .when(self.selected.is_some(), |this| {
                this.child(div().h(px(1.0)).bg(palette.divider)).child(
                    div()
                        .h(px(DIFF_HEIGHT))
                        .flex_shrink_0()
                        .child(self.pane.render(false, &palette, cx)),
                )
            })
            .relative()
            .children(self.render_conflict_dialog(&palette, direction, cx))
    }
}
