//! Scheduled tasks (`ScheduledTasksDialog.tsx`): a page over the chat with
//! the scopes on the start side (Chats, then every project) and the chosen
//! scope's tasks, each with its schedule, model, next and last run, and
//! enable, run now, edit and delete. `TaskEditor` is the create and edit
//! dialog (`ScheduledTaskEditorDialog.tsx`).
//!
//! Ownership follows the reference: the list on screen belongs to the scope
//! it was read for (`list_owner`), and every row action writes there even if
//! the selection moved since. Reads are numbered per scope, so only the
//! newest read of the scope on screen lands. A failed read keeps the list
//! out of view behind an error; it is never shown as "no tasks".
//!
//! Not ported: the prompt's command, file and snippet autocomplete, the
//! date and time pickers (dates and times are typed), the branch picker
//! (the base branch is typed), opening a loop's file from Edit, and the
//! mobile layout.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::{
    AppContext, Context, Entity, ParentElement, SharedString, Subscription, Task, WeakEntity,
    Window,
};
use openchamber_native_core::ids::{RuntimeId, SessionId};
use openchamber_native_core::schedule::{DraftDefaults, ScheduleKind, SessionMode, TaskDraft};
use openchamber_native_core::send_config::{AgentEntry, CatalogProvider};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::scheduled_tasks::{
    self, CHATS_SCOPE, ScheduledTask, ScheduledTaskError,
};
use serde_json::Value;

use crate::locale;
use crate::network::NetworkTask;
use crate::workspace::Workspace;

/// A row action's request, boxed so every action shares one dispatch path.
type Write<T> = Pin<Box<dyn Future<Output = Result<T, ScheduledTaskError>> + Send>>;

/// `scheduled-task-ran` reloads coalesce over this window.
const RAN_DEBOUNCE: Duration = Duration::from_millis(400);

/// Where tasks can live: Chats or a registered project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Scope {
    /// The settings project id, or `openchamber:chats`.
    pub id: String,
    pub label: SharedString,
    /// The project folder; Chats has none.
    pub path: Option<String>,
}

/// The visible list.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TaskList {
    Loading,
    Ready(Vec<ScheduledTask>),
    /// The read failed; the server's explanation when it gave one.
    Failed(String),
}

/// A toast the page raised last, kept for tests and accessibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Toast {
    pub error: bool,
    pub text: SharedString,
}

/// What a task editor offers for its model controls, captured on open.
#[derive(Clone, Debug, Default)]
pub(crate) struct ScheduleCatalog {
    pub providers: Vec<CatalogProvider>,
    /// Primary agents, as the editor's agent picker filters them.
    pub agents: Vec<AgentEntry>,
    pub defaults: DraftDefaults,
    /// Candidate target sessions: id, directory and title.
    pub sessions: Vec<(String, String, String)>,
}

pub(crate) struct ScheduledTasks {
    owner: WeakEntity<Workspace>,
    #[cfg(test)]
    fixed: Option<(ServerClient, tokio::runtime::Handle)>,
    runtime: RuntimeId,
    pub(crate) scopes: Vec<Scope>,
    pub(crate) selected: Option<String>,
    /// The scope whose tasks the list holds; row actions write there.
    list_owner: Option<String>,
    pub(crate) list: TaskList,
    requests: HashMap<String, u64>,
    load_task: Option<Task<()>>,
    ran_task: Option<Task<()>>,
    pub(crate) mutating: Option<String>,
    mutate_task: Option<Task<()>>,
    pub(crate) editor: Option<Entity<TaskEditor>>,
    pub(crate) toast: Option<Toast>,
}

fn error_text(error: &ScheduledTaskError) -> Option<String> {
    error.message().map(str::to_owned)
}

impl ScheduledTasks {
    pub(crate) fn new(owner: WeakEntity<Workspace>, runtime: RuntimeId) -> Self {
        Self {
            owner,
            #[cfg(test)]
            fixed: None,
            runtime,
            scopes: Vec::new(),
            selected: None,
            list_owner: None,
            list: TaskList::Loading,
            requests: HashMap::new(),
            load_task: None,
            ran_task: None,
            mutating: None,
            mutate_task: None,
            editor: None,
            toast: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_network(
        mut self,
        client: ServerClient,
        network: tokio::runtime::Handle,
    ) -> Self {
        self.fixed = Some((client, network));
        self
    }

    pub(crate) fn runtime(&self) -> &RuntimeId {
        &self.runtime
    }

    pub(crate) fn owner_entity(&self) -> Option<Entity<Workspace>> {
        self.owner.upgrade()
    }

    fn network(&self, cx: &gpui_kit::App) -> Option<(ServerClient, tokio::runtime::Handle)> {
        #[cfg(test)]
        if let Some(fixed) = &self.fixed {
            return Some(fixed.clone());
        }
        let workspace = self.owner.upgrade()?;
        let workspace = workspace.read(cx);
        if workspace.runtime != self.runtime {
            return None;
        }
        let connection = workspace.connection.as_ref()?;
        Some((connection.client.clone()?, connection.network.handle()))
    }

    fn notify_toast(
        &mut self,
        error: bool,
        text: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toast = Some(Toast {
            error,
            text: text.clone(),
        });
        let notification = if error {
            Notification::error(text)
        } else {
            Notification::success(text)
        };
        window.push_notification(notification, cx);
    }

    /// A failure's toast: the server's words, else the action's own message.
    fn fail(
        &mut self,
        error: &ScheduledTaskError,
        fallback: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = error_text(error)
            .map(SharedString::from)
            .unwrap_or_else(|| locale::strings(cx).get(fallback));
        self.notify_toast(true, text, window, cx);
    }

    /// Takes the scopes as the page opens, on the active project when there
    /// is one, else the first project, else Chats.
    pub(crate) fn open(
        &mut self,
        scopes: Vec<Scope>,
        active: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let preferred = active
            .filter(|id| scopes.iter().any(|scope| &scope.id == id))
            .or_else(|| {
                scopes
                    .iter()
                    .find(|scope| scope.id != CHATS_SCOPE)
                    .or(scopes.first())
                    .map(|scope| scope.id.clone())
            });
        self.scopes = scopes;
        // The workspace that opens the page is still being updated; the
        // connection is read once that update ends.
        let page = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = page.update(cx, |page, cx| page.select_scope(preferred, cx));
        });
    }

    pub(crate) fn select_scope(&mut self, scope: Option<String>, cx: &mut Context<Self>) {
        self.selected = scope.clone();
        match scope {
            Some(scope) => self.reload(scope, false, cx),
            None => {
                self.list_owner = None;
                self.list = TaskList::Ready(Vec::new());
                cx.notify();
            }
        }
    }

    pub(crate) fn tasks(&self) -> &[ScheduledTask] {
        match &self.list {
            TaskList::Ready(tasks) => tasks,
            _ => &[],
        }
    }

    pub(crate) fn task(&self, id: &str) -> Option<&ScheduledTask> {
        self.tasks().iter().find(|task| task.id == id)
    }

    /// Reads `scope`'s tasks. Only the newest read for a scope lands, and
    /// only while that scope is on screen; a silent read keeps the list.
    pub(crate) fn reload(&mut self, scope: String, silent: bool, cx: &mut Context<Self>) {
        if self.selected.as_deref() != Some(scope.as_str()) {
            return;
        }
        let request = {
            let counter = self.requests.entry(scope.clone()).or_default();
            *counter += 1;
            *counter
        };
        if !silent {
            self.list = TaskList::Loading;
        }
        let Some((client, network)) = self.network(cx) else {
            self.list = TaskList::Failed(String::new());
            cx.notify();
            return;
        };
        let read_scope = scope.clone();
        let task = NetworkTask::spawn_on(&network, async move {
            client.scheduled_tasks(&read_scope).await
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.finish_load(scope, request, silent, result, cx);
            });
        }));
        cx.notify();
    }

    fn finish_load(
        &mut self,
        scope: String,
        request: u64,
        silent: bool,
        result: Result<Vec<ScheduledTask>, ScheduledTaskError>,
        cx: &mut Context<Self>,
    ) {
        let latest = self.requests.get(&scope) == Some(&request);
        if !latest || self.selected.as_deref() != Some(scope.as_str()) {
            return;
        }
        match result {
            Ok(mut tasks) => {
                tasks.sort_by(|a, b| {
                    openchamber_native_core::schedule::compare_tasks(
                        (a.enabled, &a.name, a.state.next_run_at),
                        (b.enabled, &b.name, b.state.next_run_at),
                    )
                });
                self.list_owner = Some(scope);
                self.list = TaskList::Ready(tasks);
            }
            Err(error) => {
                let text = error_text(&error).unwrap_or_default();
                let strings = locale::strings(cx);
                let message = if text.is_empty() {
                    strings.get("sessions.scheduledTasks.dialog.toast.loadFailed")
                } else {
                    text.clone().into()
                };
                self.toast = Some(Toast {
                    error: true,
                    text: message,
                });
                // A silent refresh that fails keeps the list it had.
                if !silent || !matches!(self.list, TaskList::Ready(_)) {
                    self.list_owner = Some(scope);
                    self.list = TaskList::Failed(text);
                }
            }
        }
        cx.notify();
    }

    /// `scheduled-task-ran` for `project_id`: a quiet reload shortly after.
    pub(crate) fn task_ran(&mut self, project_id: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() != Some(project_id) {
            return;
        }
        let scope = project_id.to_owned();
        self.ran_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RAN_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                this.ran_task = None;
                this.reload(scope, true, cx);
            });
        }));
    }

    /// Runs `write` against the list's own scope for task `task_id`, then
    /// reads the list again whatever the outcome.
    fn mutate<T: Send + 'static>(
        &mut self,
        task_id: &str,
        write: impl FnOnce(ServerClient, String) -> Write<T>,
        done: impl FnOnce(
            &mut Self,
            &str,
            Result<T, ScheduledTaskError>,
            &mut Window,
            &mut Context<Self>,
        ) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scope) = self.list_owner.clone() else {
            return;
        };
        if self.mutating.is_some() {
            return;
        }
        let Some((client, network)) = self.network(cx) else {
            return;
        };
        self.mutating = Some(task_id.to_owned());
        let task = NetworkTask::spawn_on(&network, write(client, scope.clone()));
        let id = task_id.to_owned();
        self.mutate_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.mutating = None;
                this.mutate_task = None;
                if let Some(result) = result {
                    done(this, &id, result, window, cx);
                }
                this.reload(scope, true, cx);
            });
        }));
        cx.notify();
    }

    /// Enables or pauses a task. A repository loop changes through its file.
    pub(crate) fn toggle_enabled(
        &mut self,
        task_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self.task(task_id).cloned() else {
            return;
        };
        if self.mutating.is_some() {
            return;
        }
        let enabled = !task.enabled;
        // Shown at once; the reload brings back a reason the server keeps.
        if let TaskList::Ready(tasks) = &mut self.list
            && let Some(row) = tasks.iter_mut().find(|row| row.id == task.id)
        {
            row.enabled = enabled;
            row.loop_approval = None;
        }
        let is_loop = task.loop_file.is_some();
        let mut raw = task.raw.clone();
        raw.insert("enabled".into(), Value::Bool(enabled));
        let id = task.id.clone();
        self.mutate(
            task_id,
            move |client, scope| {
                Box::pin(async move {
                    if is_loop {
                        client.set_loop_task_enabled(&scope, &id, enabled).await
                    } else {
                        client
                            .save_scheduled_task(&scope, Value::Object(raw))
                            .await
                            .map(|_| ())
                    }
                })
            },
            |this, _, result, window, cx| {
                if let Err(error) = result {
                    this.fail(
                        &error,
                        "sessions.scheduledTasks.dialog.toast.updateFailed",
                        window,
                        cx,
                    );
                }
            },
            window,
            cx,
        );
    }

    /// Asks before deleting; a loop's file goes with it.
    pub(crate) fn request_delete(
        &mut self,
        task_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self.task(task_id).cloned() else {
            return;
        };
        let strings = locale::strings(cx).clone();
        let description = strings.format(
            if task.loop_file.is_some() {
                "sessions.scheduledTasks.dialog.confirm.deleteLoopFile"
            } else {
                "sessions.scheduledTasks.dialog.confirm.deleteTask"
            },
            &[("taskName", &task.name)],
        );
        let page = cx.entity().downgrade();
        let id = task.id.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            crate::scheduled_tasks_view::delete_dialog(
                dialog,
                description.clone(),
                page.clone(),
                id.clone(),
                cx,
            )
        });
    }

    pub(crate) fn delete(&mut self, task_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = self.task(task_id).cloned() else {
            return;
        };
        let is_loop = task.loop_file.is_some();
        let id = task.id.clone();
        self.mutate(
            task_id,
            move |client, scope| {
                Box::pin(async move {
                    if is_loop {
                        client.delete_loop_task_file(&scope, &id).await
                    } else {
                        client.delete_scheduled_task(&scope, &id).await.map(|_| ())
                    }
                })
            },
            |this, _, result, window, cx| match result {
                Ok(()) => {
                    let text =
                        locale::strings(cx).get("sessions.scheduledTasks.dialog.toast.deleted");
                    this.notify_toast(false, text, window, cx);
                }
                Err(error) => this.fail(
                    &error,
                    "sessions.scheduledTasks.dialog.toast.deleteFailed",
                    window,
                    cx,
                ),
            },
            window,
            cx,
        );
    }

    /// Starts a run now and opens its session.
    pub(crate) fn run_now(&mut self, task_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = self.task(task_id).cloned() else {
            return;
        };
        let id = task.id.clone();
        let started_at = task.state.last_run_at;
        let last_session = task.state.last_session_id.clone();
        self.mutate(
            task_id,
            move |client, scope| {
                Box::pin(async move { client.run_scheduled_task(&scope, &id).await })
            },
            move |this, _, result, window, cx| {
                let strings = locale::strings(cx).clone();
                match result {
                    Ok(started) => {
                        let key = if started.persist_error.is_some() {
                            "sessions.scheduledTasks.dialog.toast.startedPersistWarning"
                        } else {
                            "sessions.scheduledTasks.dialog.toast.started"
                        };
                        this.notify_toast(false, strings.get(key), window, cx);
                        if let Some(session) = started.session_id {
                            this.open_session(session, started.directory, window, cx);
                        }
                    }
                    Err(ScheduledTaskError::Busy(busy)) => {
                        let text = if busy == "queued" {
                            strings.get("sessions.scheduledTasks.dialog.toast.alreadyQueued")
                        } else if let Some(at) = started_at {
                            strings.format(
                                "sessions.scheduledTasks.dialog.toast.alreadyRunningSince",
                                &[("time", &crate::scheduled_tasks_view::clock_time(at, cx))],
                            )
                        } else {
                            strings.get("sessions.scheduledTasks.dialog.toast.alreadyRunning")
                        };
                        this.toast = Some(Toast {
                            error: false,
                            text: text.clone(),
                        });
                        let mut notification = Notification::info(text);
                        if busy == "running"
                            && let Some(session) = last_session.clone()
                        {
                            let page = cx.entity().downgrade();
                            let label =
                                strings.get("sessions.scheduledTasks.dialog.actions.openSession");
                            notification = notification.action(move |_, _, _| {
                                let page = page.clone();
                                let session = session.clone();
                                gpui_kit::component::button::Button::new("scheduled-task-busy-open")
                                    .label(label.clone())
                                    .on_click(move |_, window, cx| {
                                        let _ = page.update(cx, |page, cx| {
                                            page.open_session(session.clone(), None, window, cx)
                                        });
                                    })
                            });
                        }
                        window.push_notification(notification, cx);
                    }
                    Err(error) => this.fail(
                        &error,
                        "sessions.scheduledTasks.dialog.toast.runFailed",
                        window,
                        cx,
                    ),
                }
            },
            window,
            cx,
        );
    }

    /// The session of a task's latest run.
    pub(crate) fn open_last_run(
        &mut self,
        task_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self
            .task(task_id)
            .and_then(|task| task.state.last_session_id.clone())
        {
            self.open_session(session, None, window, cx);
        }
    }

    /// Leaves the page for a session, which may not be listed yet.
    fn open_session(
        &mut self,
        session: String,
        _directory: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.owner.upgrade() else {
            return;
        };
        let id = SessionId::new(session);
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_session_by_id(id, window, cx);
            });
        });
    }

    /// Opens the editor for a new task (`None`) or a stored one.
    pub(crate) fn open_editor(
        &mut self,
        task_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = match task_id {
            Some(id) => match self.task(id) {
                Some(task) if task.loop_file.is_none() => Some(task.clone()),
                _ => return,
            },
            None => None,
        };
        let Some(scope) = (if task.is_some() {
            self.list_owner.clone()
        } else {
            self.selected.clone()
        }) else {
            return;
        };
        let catalog = self
            .owner
            .upgrade()
            .map(|workspace| workspace.read(cx).schedule_catalog())
            .unwrap_or_default();
        let page = cx.entity().downgrade();
        let scopes = self.scopes.clone();
        let network = self.network(cx);
        let editor = cx.new(|cx| {
            let mut editor = TaskEditor::new(page, task, scope, scopes, catalog, window, cx);
            editor.network = network;
            editor
        });
        self.editor = Some(editor.clone());
        editor.update(cx, |editor, cx| editor.load_targets(cx));
        let page = cx.entity().downgrade();
        let title = locale::strings(cx).get(if task_id.is_some() {
            "sessions.scheduledTasks.editor.title.edit"
        } else {
            "sessions.scheduledTasks.editor.title.new"
        });
        window.open_dialog(cx, move |dialog, _, _| {
            let page = page.clone();
            dialog
                .w(gpui_kit::px(720.0))
                .title(title.clone())
                .on_close(move |_, _, cx| {
                    let _ = page.update(cx, |page, cx| {
                        page.editor = None;
                        cx.notify();
                    });
                })
                .child(editor.clone())
        });
        cx.notify();
    }

    /// Saves an editor's task into `target`. A stored task moved to another
    /// scope is written there first and deleted from its old scope next, so
    /// a failed delete leaves two copies, never none.
    pub(crate) fn save(
        &mut self,
        payload: Value,
        target: String,
        original: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<(), ScheduledTaskError>>> {
        let (client, network) = self.network(cx)?;
        let task = NetworkTask::spawn_on(&network, async move {
            match original {
                Some((owner, task_id)) if owner != target => {
                    let mut moved = payload;
                    if let Some(object) = moved.as_object_mut() {
                        object.remove("id");
                        object.remove("state");
                    }
                    client.save_scheduled_task(&target, moved).await?;
                    let left = client
                        .delete_scheduled_task(&owner, &task_id)
                        .await
                        .is_err();
                    Ok(left)
                }
                _ => client
                    .save_scheduled_task(&target, payload)
                    .await
                    .map(|_| false),
            }
        });
        Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await.unwrap_or(Err(ScheduledTaskError::Transport(
                openchamber_native_transport::error::TransportError::Cancelled,
            )));
            let _ = this.update_in(cx, |this, window, cx| {
                if let Ok(left_original) = &result {
                    let strings = locale::strings(cx).clone();
                    if *left_original {
                        this.notify_toast(
                            true,
                            strings.get("sessions.scheduledTasks.dialog.toast.moveLeftOriginal"),
                            window,
                            cx,
                        );
                    }
                    this.notify_toast(
                        false,
                        strings.get("sessions.scheduledTasks.dialog.toast.saved"),
                        window,
                        cx,
                    );
                    if let Some(scope) = this.selected.clone() {
                        this.reload(scope, false, cx);
                    }
                }
            });
            result.map(|_| ())
        }))
    }
}

/// Which sessions the target picker may offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Eligibility {
    Idle,
    Loading,
    Ready(HashSet<String>),
    Failed,
}

/// The editor's open picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditorMenu {
    Scope,
    SessionMode,
    Target,
    Kind,
    Timezone,
    Model,
    Variant,
    Agent,
}

pub(crate) struct TaskEditor {
    page: WeakEntity<ScheduledTasks>,
    /// The page's connection when the editor opened, for the target read.
    network: Option<(ServerClient, tokio::runtime::Handle)>,
    /// The scope and id of the stored task, when editing one.
    original: Option<(String, String)>,
    pub(crate) scope: String,
    pub(crate) scopes: Vec<Scope>,
    pub(crate) draft: TaskDraft,
    state: Option<Value>,
    pub(crate) catalog: ScheduleCatalog,
    pub(crate) eligibility: Eligibility,
    eligibility_task: Option<Task<()>>,
    pub(crate) menu: Option<EditorMenu>,
    pub(crate) saving: bool,
    save_task: Option<Task<()>>,
    pub(crate) name: Entity<InputState>,
    pub(crate) prompt: Entity<TextareaState>,
    pub(crate) cron: Entity<InputState>,
    pub(crate) once_date: Entity<InputState>,
    pub(crate) once_time: Entity<InputState>,
    pub(crate) base_branch: Entity<InputState>,
    pub(crate) budget: Entity<InputState>,
    pub(crate) filter: Entity<InputState>,
    pub(crate) times: Vec<Entity<InputState>>,
    pub(crate) error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
    time_subscriptions: Vec<Subscription>,
}

impl TaskEditor {
    #[allow(clippy::too_many_arguments)]
    fn new(
        page: WeakEntity<ScheduledTasks>,
        task: Option<ScheduledTask>,
        scope: String,
        scopes: Vec<Scope>,
        catalog: ScheduleCatalog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft = match &task {
            Some(task) => scheduled_tasks::task_draft(task, &catalog.defaults),
            None => TaskDraft::new_task(&catalog.defaults),
        };
        let input = |value: &str, window: &mut Window, cx: &mut Context<Self>| {
            let value = value.to_owned();
            cx.new(|cx| {
                let mut input = InputState::new(window, cx);
                input.set_value(value, window, cx);
                input
            })
        };
        let name = input(&draft.name, window, cx);
        let cron = input(&draft.cron, window, cx);
        let once_date = input(&draft.once_date, window, cx);
        let once_time = input(&draft.once_time, window, cx);
        let base_branch = input(&draft.base_branch, window, cx);
        let budget = input(
            &draft
                .goal_token_budget
                .map(|budget| budget.to_string())
                .unwrap_or_default(),
            window,
            cx,
        );
        let filter = input("", window, cx);
        let prompt_value = draft.prompt.clone();
        let prompt = cx.new(|cx| {
            let mut prompt = TextareaState::new(window, cx).auto_grow(6, 12);
            prompt.set_value(prompt_value, window, cx);
            prompt
        });
        let watch = |entity: &Entity<InputState>,
                     apply: fn(&mut TaskDraft, String),
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.subscribe_in(entity, window, move |this, input, event, _, cx| {
                if let InputEvent::Change = event {
                    let value = input.read(cx).value().to_string();
                    apply(&mut this.draft, value);
                    this.error = None;
                    cx.notify();
                }
            })
        };
        let subscriptions = vec![
            watch(&name, |draft, value| draft.name = value, window, cx),
            watch(&cron, |draft, value| draft.cron = value, window, cx),
            watch(
                &once_date,
                |draft, value| draft.once_date = value,
                window,
                cx,
            ),
            watch(
                &once_time,
                |draft, value| draft.once_time = value,
                window,
                cx,
            ),
            watch(
                &base_branch,
                |draft, value| draft.base_branch = value,
                window,
                cx,
            ),
            watch(
                &budget,
                |draft, value| {
                    if let Ok(budget) = value.trim().parse::<i64>()
                        && budget > 0
                    {
                        draft.goal_token_budget = Some(budget);
                    }
                },
                window,
                cx,
            ),
            watch(&filter, |_, _| {}, window, cx),
            cx.subscribe_in(&prompt, window, |this, input, event, _, cx| {
                if let InputEvent::Change = event {
                    this.draft.prompt = input.read(cx).value().to_string();
                    this.error = None;
                    cx.notify();
                }
            }),
        ];
        let mut editor = Self {
            page,
            network: None,
            original: task.as_ref().map(|task| (scope.clone(), task.id.clone())),
            scope,
            scopes,
            draft,
            state: task
                .as_ref()
                .and_then(|task| task.raw.get("state").cloned()),
            catalog,
            eligibility: Eligibility::Idle,
            eligibility_task: None,
            menu: None,
            saving: false,
            save_task: None,
            name,
            prompt,
            cron,
            once_date,
            once_time,
            base_branch,
            budget,
            filter,
            times: Vec::new(),
            error: None,
            _subscriptions: subscriptions,
            time_subscriptions: Vec::new(),
        };
        editor.rebuild_times(window, cx);
        editor
    }

    /// One input per time row, rebuilt when rows are added or removed.
    fn rebuild_times(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.times.clear();
        self.time_subscriptions.clear();
        for (index, time) in self.draft.times.clone().into_iter().enumerate() {
            let input = cx.new(|cx| {
                let mut input = InputState::new(window, cx);
                input.set_value(time, window, cx);
                input
            });
            self.time_subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this, input, event, _, cx| {
                    if let InputEvent::Change = event
                        && let Some(slot) = this.draft.times.get_mut(index)
                    {
                        *slot = input.read(cx).value().to_string();
                        this.error = None;
                        cx.notify();
                    }
                },
            ));
            self.times.push(input);
        }
    }

    pub(crate) fn add_time(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft.add_time();
        self.rebuild_times(window, cx);
        cx.notify();
    }

    pub(crate) fn remove_time(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.draft.remove_time(index);
        self.rebuild_times(window, cx);
        cx.notify();
    }

    pub(crate) fn set_cron(
        &mut self,
        expression: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cron.update(cx, |input, cx| {
            input.set_value(expression.to_owned(), window, cx)
        });
        self.draft.cron = expression.to_owned();
        cx.notify();
    }

    pub(crate) fn toggle_menu(
        &mut self,
        menu: EditorMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = (self.menu != Some(menu)).then_some(menu);
        self.filter
            .update(cx, |input, cx| input.set_value("", window, cx));
        if self
            .menu
            .is_some_and(|menu| matches!(menu, EditorMenu::Timezone | EditorMenu::Model))
        {
            self.filter.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    pub(crate) fn set_kind(
        &mut self,
        kind: ScheduleKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.draft.set_kind(kind);
        if kind == ScheduleKind::Cron {
            let cron = self.draft.cron.clone();
            self.cron
                .update(cx, |input, cx| input.set_value(cron, window, cx));
        }
        self.menu = None;
        cx.notify();
    }

    /// Another scope starts without the target session and base branch
    /// picked in the old one.
    pub(crate) fn set_scope(&mut self, scope: String, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        if scope == self.scope {
            cx.notify();
            return;
        }
        self.scope = scope;
        self.draft.target_session_id.clear();
        self.draft.base_branch.clear();
        self.base_branch
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.eligibility = Eligibility::Loading;
        self.load_targets(cx);
    }

    pub(crate) fn set_session_mode(&mut self, mode: SessionMode, cx: &mut Context<Self>) {
        self.draft.session_mode = mode;
        self.menu = None;
        cx.notify();
    }

    pub(crate) fn scope_path(&self) -> Option<&str> {
        self.scopes
            .iter()
            .find(|scope| scope.id == self.scope)
            .and_then(|scope| scope.path.as_deref())
    }

    /// Asks the scheduler which sessions this scope may target. A failed
    /// read is an error with a retry, never an empty list.
    pub(crate) fn load_targets(&mut self, cx: &mut Context<Self>) {
        if self.catalog.sessions.is_empty() {
            self.eligibility = Eligibility::Idle;
            return;
        }
        let Some((client, network)) = self.network.clone() else {
            self.eligibility = Eligibility::Failed;
            return;
        };
        if !matches!(self.eligibility, Eligibility::Ready(_)) {
            self.eligibility = Eligibility::Loading;
        }
        let scope = self.scope.clone();
        let candidates: Vec<(String, String)> = self
            .catalog
            .sessions
            .iter()
            .map(|(id, directory, _)| (id.clone(), directory.clone()))
            .collect();
        let task = NetworkTask::spawn_on(&network, async move {
            client.eligible_scheduled_targets(&scope, &candidates).await
        });
        let asked = self.scope.clone();
        self.eligibility_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.scope != asked {
                    return;
                }
                this.eligibility = match result {
                    Some(Ok(ids)) => Eligibility::Ready(ids),
                    _ => Eligibility::Failed,
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The sessions the target picker lists.
    pub(crate) fn target_sessions(&self) -> Vec<&(String, String, String)> {
        match &self.eligibility {
            Eligibility::Ready(ids) => self
                .catalog
                .sessions
                .iter()
                .filter(|(id, _, _)| ids.contains(id))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The selected model's efforts; none when the catalog does not list it.
    pub(crate) fn variants(&self) -> Vec<String> {
        self.catalog
            .providers
            .iter()
            .find(|provider| provider.id == self.draft.provider_id)
            .and_then(|provider| {
                provider
                    .models
                    .iter()
                    .find(|model| model.id == self.draft.model_id)
            })
            .map(|model| model.variants.clone())
            .unwrap_or_default()
    }

    pub(crate) fn set_model(&mut self, provider: String, model: String, cx: &mut Context<Self>) {
        self.draft.provider_id = provider;
        self.draft.model_id = model;
        self.draft.variant.clear();
        self.menu = None;
        cx.notify();
    }

    /// Validates, then saves through the page. The dialog stays open with a
    /// failure's message; it closes once the save landed.
    pub(crate) fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.menu = None;
        let strings = locale::strings(cx).clone();
        if let Some(key) = self.draft.validate() {
            let text = strings.get(key);
            self.error = Some(text.clone());
            window.push_notification(Notification::error(text), cx);
            cx.notify();
            return;
        }
        let mut draft = self.draft.clone();
        // A model the editor does not offer variants for keeps none.
        if self.variants().is_empty() {
            draft.variant.clear();
        }
        let state = (draft.session_mode == SessionMode::New)
            .then_some(self.state.as_ref())
            .flatten();
        let payload = scheduled_tasks::task_payload(&draft, state);
        let Some(page) = self.page.upgrade() else {
            return;
        };
        let target = self.scope.clone();
        let original = self.original.clone();
        let Some(task) = page.update(cx, |page, cx| {
            page.save(payload, target, original, window, cx)
        }) else {
            return;
        };
        self.saving = true;
        self.error = None;
        self.save_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(()) => window.close_dialog(cx),
                    Err(error) => {
                        let text = error.message().map(SharedString::from).unwrap_or_else(|| {
                            locale::strings(cx)
                                .get("sessions.scheduledTasks.editor.toast.saveFailed")
                        });
                        this.error = Some(text.clone());
                        window.push_notification(Notification::error(text), cx);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.saving {
            window.close_dialog(cx);
        }
    }
}

impl Workspace {
    /// What a task editor offers: the config directory's models and primary
    /// agents, the defaults a send would use, and the unarchived sessions.
    pub(crate) fn schedule_catalog(&self) -> ScheduleCatalog {
        let entry = self
            .config_directory()
            .and_then(|directory| self.send_catalog.directory(&directory));
        let providers = entry
            .and_then(|entry| entry.providers.ready())
            .cloned()
            .unwrap_or_default();
        let agents = entry
            .and_then(|entry| entry.agents.ready())
            .map(|agents| {
                agents
                    .iter()
                    .filter(|agent| {
                        !agent.hidden
                            && agent.mode.is_none_or(|mode| {
                                mode != openchamber_native_core::send_config::AgentMode::Subagent
                            })
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let selection = self.send_selection().map(|(_, selection)| selection);
        let model = selection
            .as_ref()
            .and_then(|selection| selection.model.clone())
            .filter(|model| !(model.provider_id == "openchamber" && model.model_id == "auto"));
        let now = chrono::Utc::now().timestamp();
        let defaults = DraftDefaults {
            provider_id: model
                .as_ref()
                .map(|model| model.provider_id.clone())
                .unwrap_or_default(),
            model_id: model
                .as_ref()
                .map(|model| model.model_id.clone())
                .unwrap_or_default(),
            variant: model
                .as_ref()
                .and_then(|model| model.variant.clone())
                .unwrap_or_default(),
            agent: selection
                .and_then(|selection| selection.agent_name)
                .unwrap_or_default(),
            timezone: crate::time_zones::local_zone_name(),
            today: crate::time_zones::local_today(now),
        };
        let sessions = self
            .sessions
            .active()
            .iter()
            .filter(|session| session.time.archived.is_none())
            .map(|session| {
                (
                    session.id.as_str().to_owned(),
                    session.resolved_directory().unwrap_or_default(),
                    session.title.clone(),
                )
            })
            .collect();
        ScheduleCatalog {
            providers,
            agents,
            defaults,
            sessions,
        }
    }

    /// The scopes in sidebar order: Chats, then every registered project.
    pub(crate) fn schedule_scopes(&self, cx: &gpui_kit::App) -> Vec<Scope> {
        let strings = locale::strings(cx);
        let mut scopes = vec![Scope {
            id: CHATS_SCOPE.to_owned(),
            label: strings.get("sessions.scheduledTasks.dialog.project.chats"),
            path: None,
        }];
        scopes.extend(self.topology.projects.iter().map(|project| Scope {
            id: project.id.as_str().to_owned(),
            label: self.project_label(&project.path),
            path: Some(project.path.clone()),
        }));
        scopes
    }

    pub(crate) fn toggle_scheduled_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        self.scheduled_tasks_open = !self.scheduled_tasks_open;
        if !self.scheduled_tasks_open {
            self.focus_composer(window, cx);
            cx.notify();
            return;
        }
        self.archive_open = false;
        self.source_board_open = false;
        if self
            .scheduled_tasks
            .as_ref()
            .is_some_and(|page| page.read(cx).runtime() != &self.runtime)
        {
            self.scheduled_tasks = None;
        }
        let runtime = self.runtime.clone();
        let owner = cx.entity().downgrade();
        let page = self
            .scheduled_tasks
            .get_or_insert_with(|| cx.new(|_| ScheduledTasks::new(owner, runtime)))
            .clone();
        let scopes = self.schedule_scopes(cx);
        let active = self
            .send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.active_project_id.clone());
        page.update(cx, |page, cx| page.open(scopes, active, cx));
        cx.notify();
    }

    /// Leaves for a session by id, reading it first when the list does not
    /// have it yet (a run creates its session after the list last loaded).
    pub(crate) fn open_session_by_id(
        &mut self,
        id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scheduled_tasks_open = false;
        // The ordinary navigation owner also resolves unknown records and guards
        // its lookup against host changes and subsequent navigation.
        self.select_session(id, window, cx);
        cx.notify();
    }
}
