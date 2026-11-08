//! The Project knowledge panel (`project-context/ProjectNotesTodoPanel.tsx`):
//! notes, todos and plans of the panel's owner, in tabs that count the
//! search's matches, an open note or plan in place of its list, and the
//! sends that start a session from a todo or a plan.
//!
//! The owner is the project the panel's directory belongs to (a worktree
//! resolves to its project, a chat to the Chats root), captured when the
//! view is made: every write goes to that owner even after the panel moved
//! on. The server holds the data. Reads keep the last good snapshot, so a
//! failure never blanks the panel; it toasts once, and only while nothing
//! has loaded. Todo changes show at once and send that one todo; a failed
//! one reads the list again. An open note or plan keeps its own draft,
//! saved after a pause and when it is left, and adopts a newer stored
//! text only while it is untouched since its last save. A failed save
//! keeps the draft. Deletes of notes and plans wait out an undo window.
//!
//! Not ported: agent memory, pinning to the session, plan import from a
//! file, the formatted Markdown editors (notes and plans edit as plain
//! text), drag reorder, the wide side-by-side layout, the send dialog's
//! model choice (sends use the composer's model and agent) and sends to a
//! new worktree.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::{AppContext, Context, Entity, SharedString, Subscription, Task, Window};
use openchamber_native_core::ids::ProjectId;
use openchamber_native_core::knowledge::{
    self, Knowledge, NOTE_BODY_MAX_LENGTH, PLAN_TITLE_MAX_LENGTH, Section, TODO_TEXT_MAX_LENGTH,
    Todo,
};
use openchamber_native_transport::project_context::{KnowledgeError, PlanDocument, TodoChange};

use crate::files_view::PanelScope;
use crate::locale;

/// `NOTE_SAVE_DEBOUNCE_MS` and PlanView's autosave delay.
const NOTE_SAVE_DELAY: Duration = Duration::from_millis(400);
const PLAN_SAVE_DELAY: Duration = Duration::from_millis(350);
/// `UNDO_WINDOW_MS`.
pub(crate) const UNDO_WINDOW: Duration = Duration::from_secs(5);
/// `PLAN_LIST_REFRESH_MS`: agents tick plan steps without announcing it.
const PLAN_REFRESH: Duration = Duration::from_secs(10);

/// Whose knowledge the panel shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KnowledgeOwner {
    /// The path-derived id the server stores under.
    pub project_id: String,
    pub path: String,
    pub label: SharedString,
}

impl KnowledgeOwner {
    pub(crate) fn new(path: &str, label: SharedString) -> Option<Self> {
        Some(Self {
            project_id: ProjectId::from_path(path)?.as_str().to_owned(),
            path: path.to_owned(),
            label,
        })
    }
}

/// An open plan's document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlanLoad {
    Loading,
    Ready(Box<PlanDocument>),
    /// The plan or its file is gone.
    Missing,
    Failed(String),
}

/// A send from a todo or a plan, waiting for the workspace to carry it out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Dispatch {
    /// `plan.todo.*` in a new session of the owner.
    Todo { id: String, text: String },
    /// `plan.implement.*` or `plan.improve.*` for an open plan.
    Plan { id: String, implement: bool },
}

/// What the panel asks of the workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KnowledgeEvent {
    Dispatch(Dispatch),
    /// "Send to current session": text for the composer.
    AppendToComposer(String),
    OpenSession(String),
}

impl gpui_kit::EventEmitter<KnowledgeEvent> for KnowledgePanel {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Toast {
    pub error: bool,
    pub text: SharedString,
}

pub(crate) struct KnowledgePanel {
    pub(crate) scope: PanelScope,
    pub(crate) owner: KnowledgeOwner,
    /// The last good snapshot; `None` until one loaded.
    pub(crate) knowledge: Option<Knowledge>,
    pub(crate) loading: bool,
    pub(crate) load_error: Option<String>,
    load_reported: bool,
    load_revision: u64,
    load_task: Option<Task<()>>,
    last_read: Option<Instant>,
    pub(crate) section: Section,
    pub(crate) search_open: bool,
    pub(crate) search: Entity<InputState>,
    pub(crate) query: String,
    pub(crate) open: Option<(Section, String)>,
    pub(crate) note_input: Entity<TextareaState>,
    pub(crate) todo_input: Entity<InputState>,
    pub(crate) plan_input: Entity<InputState>,
    pub(crate) note_editor: Entity<TextareaState>,
    /// The open note's text as last saved or adopted.
    note_saved: String,
    note_timer: Option<Task<()>>,
    adopted_note: Option<String>,
    pub(crate) note_saving: bool,
    pub(crate) plan: Option<PlanLoad>,
    pub(crate) plan_editor: Entity<TextareaState>,
    plan_saved: String,
    plan_timer: Option<Task<()>>,
    pub(crate) plan_saving: bool,
    /// Deletes waiting out their undo window, by entry id.
    pending_deletes: HashMap<String, Task<()>>,
    pub(crate) todo_menu: Option<String>,
    pub(crate) entry_menu: Option<String>,
    pub(crate) expanded: HashSet<String>,
    pub(crate) sending: Option<String>,
    /// Todo writes the server has not answered yet.
    pub(crate) todo_writes: usize,
    pub(crate) moving: Option<String>,
    pub(crate) toast: Option<Toast>,
    tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl KnowledgePanel {
    pub(crate) fn new(
        scope: PanelScope,
        owner: KnowledgeOwner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx));
        let note_input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 8));
        let todo_input = cx.new(|cx| InputState::new(window, cx));
        let plan_input = cx.new(|cx| InputState::new(window, cx));
        let note_editor = cx.new(|cx| TextareaState::new(window, cx).auto_grow(12, 40));
        let plan_editor = cx.new(|cx| TextareaState::new(window, cx).auto_grow(16, 60));
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, input, event, _, cx| {
                if let InputEvent::Change = event {
                    this.set_query(input.read(cx).value().to_string(), cx);
                }
            }),
            cx.subscribe_in(&note_input, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter {
                    secondary: false, ..
                } = event
                {
                    this.add_note(window, cx);
                }
            }),
            cx.subscribe_in(&todo_input, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add_todo(window, cx);
                }
            }),
            cx.subscribe_in(&plan_input, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add_plan(window, cx);
                }
            }),
            cx.subscribe_in(&note_editor, window, |this, _, event, _, cx| {
                if let InputEvent::Change = event {
                    this.schedule_note_save(cx);
                }
            }),
            cx.subscribe_in(&plan_editor, window, |this, _, event, _, cx| {
                if let InputEvent::Change = event {
                    this.schedule_plan_save(cx);
                }
            }),
        ];
        let panel = Self {
            scope,
            owner,
            knowledge: None,
            loading: false,
            load_error: None,
            load_reported: false,
            load_revision: 0,
            load_task: None,
            last_read: None,
            section: Section::Notes,
            search_open: false,
            search,
            query: String::new(),
            open: None,
            note_input,
            todo_input,
            plan_input,
            note_editor,
            note_saved: String::new(),
            note_timer: None,
            adopted_note: None,
            note_saving: false,
            plan: None,
            plan_editor,
            plan_saved: String::new(),
            plan_timer: None,
            plan_saving: false,
            pending_deletes: HashMap::new(),
            todo_menu: None,
            entry_menu: None,
            expanded: HashSet::new(),
            sending: None,
            todo_writes: 0,
            moving: None,
            toast: None,
            tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        // The side panel makes this view inside the workspace's update, and a
        // read asks the workspace for its connection; it starts once that ends.
        let view = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = view.update(cx, |view, cx| view.reload(cx));
        });
        panel
    }

    fn project_id(&self) -> String {
        self.owner.project_id.clone()
    }

    fn toast(
        &mut self,
        error: bool,
        key: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = locale::strings(cx).get(key);
        self.toast = Some(Toast {
            error,
            text: text.clone(),
        });
        window.push_notification(
            if error {
                Notification::error(text)
            } else {
                Notification::success(text)
            },
            cx,
        );
    }

    /// Reads the owner's knowledge; only the newest read lands.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        self.load_revision += 1;
        let revision = self.load_revision;
        self.last_read = Some(Instant::now());
        let Some(client) = self.scope.client(cx) else {
            self.load_error = Some(String::new());
            return;
        };
        self.loading = true;
        let project = self.project_id();
        let task = self
            .scope
            .run(async move { client.project_knowledge(&project).await });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.load_revision != revision {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(knowledge) => {
                        this.load_error = None;
                        this.load_reported = false;
                        this.adopt(knowledge, cx);
                    }
                    Err(error) => {
                        this.load_error = Some(error.message().unwrap_or_default().to_owned());
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Whether the load failure toast is still owed; once per failure, and
    /// only while nothing has loaded.
    pub(crate) fn take_load_failure(&mut self) -> bool {
        if self.load_error.is_some() && self.knowledge.is_none() && !self.load_reported {
            self.load_reported = true;
            return true;
        }
        false
    }

    /// Takes a fresh snapshot; an open entry deleted elsewhere closes.
    fn adopt(&mut self, knowledge: Knowledge, cx: &mut Context<Self>) {
        if let Some((section, id)) = &self.open {
            let present = match section {
                Section::Notes => knowledge.notes.iter().any(|note| &note.id == id),
                Section::Plans => knowledge.plans.iter().any(|plan| &plan.id == id),
                Section::Todos => true,
            };
            if !present {
                self.open = None;
                self.plan = None;
            }
        }
        self.knowledge = Some(knowledge);
        self.adopt_open_note(cx);
    }

    /// The plans list re-reads while it is on screen.
    pub(crate) fn refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        if self.section == Section::Plans
            && !self.loading
            && self
                .last_read
                .is_none_or(|read| read.elapsed() >= PLAN_REFRESH)
        {
            self.reload(cx);
        }
    }

    pub(crate) fn hidden(&self) -> HashSet<String> {
        self.pending_deletes.keys().cloned().collect()
    }

    pub(crate) fn counts(&self) -> knowledge::Counts {
        self.knowledge
            .as_ref()
            .map(|knowledge| knowledge::counts(knowledge, &self.query, &self.hidden()))
            .unwrap_or_default()
    }

    pub(crate) fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        let changed = query.trim().to_lowercase() != self.query.trim().to_lowercase();
        self.query = query;
        // The search follows to where its matches are, only as it changes.
        if changed
            && !self.query.trim().is_empty()
            && let Some(section) = knowledge::follow_search(self.section, self.counts())
        {
            self.select_section(section, cx);
        }
        cx.notify();
    }

    pub(crate) fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_open {
            self.close_search(window, cx);
            return;
        }
        self.search_open = true;
        self.search.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// Closing the search clears it, so no list stays filtered unseen.
    pub(crate) fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = false;
        self.search
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.query.clear();
        cx.notify();
    }

    pub(crate) fn select_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if self.section != section {
            self.flush_open(cx);
            self.open = None;
            self.plan = None;
        }
        self.section = section;
        self.todo_menu = None;
        self.entry_menu = None;
        cx.notify();
    }

    // Notes.

    pub(crate) fn add_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.note_input.read(cx).value().to_string();
        let body: String = body.chars().take(NOTE_BODY_MAX_LENGTH).collect();
        if body.trim().is_empty() {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        // The field clears at once and stays usable; a failure puts the text
        // back if nothing new was typed meanwhile.
        self.note_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        let project = self.project_id();
        let sent = body.clone();
        let task = self
            .scope
            .run(async move { client.create_note(&project, &sent).await });
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Some(Ok((_, knowledge))) => this.adopt(knowledge, cx),
                    _ => {
                        if this.note_input.read(cx).value().is_empty() {
                            this.note_input
                                .update(cx, |input, cx| input.set_value(body.clone(), window, cx));
                        }
                        this.toast(
                            true,
                            "rightSidebar.contextNotesTodo.toast.createNoteFailed",
                            window,
                            cx,
                        );
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Opens a note in place of the list, or closes it with `None`.
    pub(crate) fn open_note(
        &mut self,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.flush_open(cx);
        self.entry_menu = None;
        let Some(id) = id else {
            self.open = None;
            cx.notify();
            return;
        };
        let body = self
            .knowledge
            .as_ref()
            .and_then(|knowledge| knowledge.notes.iter().find(|note| note.id == id))
            .map(|note| note.body.clone())
            .unwrap_or_default();
        self.note_saved = body.clone();
        self.note_editor.update(cx, |input, cx| {
            input.set_value(body, window, cx);
            input.focus(window, cx);
        });
        self.open = Some((Section::Notes, id));
        cx.notify();
    }

    fn open_note_id(&self) -> Option<&str> {
        match &self.open {
            Some((Section::Notes, id)) => Some(id),
            _ => None,
        }
    }

    /// A newer stored body reaches an open note only while it is untouched.
    fn adopt_open_note(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.open_note_id().map(str::to_owned) else {
            return;
        };
        let Some(stored) = self
            .knowledge
            .as_ref()
            .and_then(|knowledge| knowledge.notes.iter().find(|note| note.id == id))
            .map(|note| note.body.clone())
        else {
            return;
        };
        let draft = self.note_editor.read(cx).value().to_string();
        if stored == self.note_saved || draft != self.note_saved {
            return;
        }
        self.note_saved = stored.clone();
        // Setting an input's value needs the window; the next frame applies it.
        self.adopted_note = Some(stored);
        cx.notify();
    }

    /// The stored body an untouched open note takes on its next frame.
    pub(crate) fn take_adopted_note(&mut self) -> Option<String> {
        self.adopted_note.take()
    }

    fn schedule_note_save(&mut self, cx: &mut Context<Self>) {
        if self.open_note_id().is_none() {
            return;
        }
        self.note_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTE_SAVE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.note_timer = None;
                this.save_note(cx);
            });
        }));
    }

    /// Saves the open note's draft. A blank body is never sent: the server
    /// rejects it, and deleting is its own action.
    pub(crate) fn save_note(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.open_note_id().map(str::to_owned) else {
            return;
        };
        let draft = self.note_editor.read(cx).value().to_string();
        if draft == self.note_saved || draft.trim().is_empty() {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let previous = std::mem::replace(&mut self.note_saved, draft.clone());
        self.note_saving = true;
        let project = self.project_id();
        let task = self
            .scope
            .run(async move { client.update_note(&project, &id, &draft).await });
        self.tasks.push(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.note_saving = false;
                match result {
                    Some(Ok(Some(note))) => {
                        if let Some(knowledge) = &mut this.knowledge
                            && let Some(stored) = knowledge
                                .notes
                                .iter_mut()
                                .find(|stored| stored.id == note.id)
                        {
                            *stored = note;
                        }
                    }
                    Some(Ok(None)) => {
                        // Deleted elsewhere: the next read closes the note.
                        this.reload(cx);
                    }
                    _ => {
                        // The draft stays in the editor and saves again with
                        // the next keystroke or on leaving.
                        this.note_saved = previous;
                        this.toast = Some(Toast {
                            error: true,
                            text: locale::strings(cx)
                                .get("rightSidebar.contextNotesTodo.toast.saveNotesFailed"),
                        });
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Leaving an entry saves what is still waiting for its pause.
    fn flush_open(&mut self, cx: &mut Context<Self>) {
        if self.note_timer.take().is_some() {
            self.save_note(cx);
        }
        if self.plan_timer.take().is_some() {
            self.save_plan(cx);
        }
    }

    // Todos.

    fn todos(&self) -> Vec<Todo> {
        self.knowledge
            .as_ref()
            .map(|knowledge| knowledge.todos.clone())
            .unwrap_or_default()
    }

    /// Shows `next` at once and sends `change`; a failure reads the list again.
    fn write_todo(
        &mut self,
        next: Vec<Todo>,
        change: TodoChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        if let Some(knowledge) = &mut self.knowledge {
            knowledge.todos = next;
        }
        self.todo_writes += 1;
        let project = self.project_id();
        let task = self
            .scope
            .run(async move { client.change_todo(&project, &change).await });
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.todo_writes -= 1;
                match result {
                    Some(Ok(Some(knowledge))) => this.adopt(knowledge, cx),
                    Some(Ok(None)) => this.reload(cx),
                    _ => {
                        this.toast(
                            true,
                            "rightSidebar.contextNotesTodo.toast.saveNotesFailed",
                            window,
                            cx,
                        );
                        this.reload(cx);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn add_todo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = knowledge::single_line(&self.todo_input.read(cx).value(), TODO_TEXT_MAX_LENGTH);
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.todo_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        let id = new_id("todo");
        let todo = Todo {
            id: id.clone(),
            text: text.clone(),
            completed: false,
            created_at: chrono::Utc::now().timestamp_millis(),
            plan_id: None,
            depends_on: Vec::new(),
        };
        let next = knowledge::insert_before_completed(&self.todos(), todo);
        self.write_todo(next, TodoChange::Create { id, text }, window, cx);
    }

    pub(crate) fn toggle_todo(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let todos = self.todos();
        let Some(completed) = todos
            .iter()
            .find(|todo| todo.id == id)
            .map(|todo| !todo.completed)
        else {
            return;
        };
        let Some(next) = knowledge::toggled(&todos, id, completed) else {
            return;
        };
        self.write_todo(
            next,
            TodoChange::Complete {
                id: id.to_owned(),
                completed,
            },
            window,
            cx,
        );
    }

    pub(crate) fn delete_todo(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.todo_menu = None;
        let next = self
            .todos()
            .into_iter()
            .filter(|todo| todo.id != id)
            .collect();
        self.write_todo(next, TodoChange::Delete { id: id.to_owned() }, window, cx);
    }

    /// Writes the whole list without its completed todos.
    pub(crate) fn clear_completed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let todos = self.todos();
        let next: Vec<Todo> = todos
            .iter()
            .filter(|todo| !todo.completed)
            .cloned()
            .collect();
        if next.len() == todos.len() {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        if let Some(knowledge) = &mut self.knowledge {
            knowledge.todos = next.clone();
        }
        let project = self.project_id();
        let task = self
            .scope
            .run(async move { client.save_todos(&project, &next).await });
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Some(Ok(knowledge)) => this.adopt(knowledge, cx),
                    _ => {
                        this.toast(
                            true,
                            "rightSidebar.contextNotesTodo.toast.saveNotesFailed",
                            window,
                            cx,
                        );
                        this.reload(cx);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn toggle_expanded(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_owned());
        }
        cx.notify();
    }

    pub(crate) fn toggle_todo_menu(&mut self, id: &str, cx: &mut Context<Self>) {
        self.todo_menu = (self.todo_menu.as_deref() != Some(id)).then(|| id.to_owned());
        self.entry_menu = None;
        cx.notify();
    }

    /// The todo as a fenced block appended to the composer.
    pub(crate) fn send_todo_to_current(&mut self, id: &str, cx: &mut Context<Self>) {
        self.todo_menu = None;
        let Some(todo) = self.todos().into_iter().find(|todo| todo.id == id) else {
            return;
        };
        cx.emit(KnowledgeEvent::AppendToComposer(knowledge::fenced_todo(
            &todo.text,
        )));
        cx.notify();
    }

    /// The workspace's answer to an append: whether a session took it.
    pub(crate) fn appended(&mut self, taken: bool, window: &mut Window, cx: &mut Context<Self>) {
        if taken {
            self.toast(
                false,
                "rightSidebar.contextNotesTodo.toast.sentToCurrentSession",
                window,
                cx,
            );
        } else {
            self.toast(
                true,
                "rightSidebar.contextNotesTodo.toast.noActiveSession",
                window,
                cx,
            );
        }
    }

    pub(crate) fn send_todo_to_new(&mut self, id: &str, cx: &mut Context<Self>) {
        self.todo_menu = None;
        if self.sending.is_some() {
            return;
        }
        let Some(todo) = self.todos().into_iter().find(|todo| todo.id == id) else {
            return;
        };
        self.sending = Some(todo.id.clone());
        cx.emit(KnowledgeEvent::Dispatch(Dispatch::Todo {
            id: todo.id,
            text: todo.text,
        }));
        cx.notify();
    }

    /// The workspace's answer to a send: the toast, and the send's controls
    /// free again.
    pub(crate) fn finish_dispatch(
        &mut self,
        key: Option<&'static str>,
        error: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sending = None;
        if let Some(key) = key {
            self.toast(error, key, window, cx);
        }
        cx.notify();
    }

    // Plans.

    pub(crate) fn add_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title =
            knowledge::single_line(&self.plan_input.read(cx).value(), PLAN_TITLE_MAX_LENGTH);
        let title = title.trim().to_owned();
        if title.is_empty() {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        self.plan_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        let project = self.project_id();
        let sent = title.clone();
        let task = self
            .scope
            .run(async move { client.create_plan(&project, &sent, "").await });
        // Plan creation is not optimistic: the id and file come from the
        // server, and the new plan opens once it exists.
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Some(Ok((plan, knowledge))) => {
                        this.adopt(knowledge, cx);
                        if this.section == Section::Plans {
                            this.open_plan(Some(plan.id), window, cx);
                        }
                    }
                    _ => {
                        if this.plan_input.read(cx).value().is_empty() {
                            this.plan_input
                                .update(cx, |input, cx| input.set_value(title.clone(), window, cx));
                        }
                        this.toast(
                            true,
                            "rightSidebar.contextNotesTodo.toast.createPlanFailed",
                            window,
                            cx,
                        );
                    }
                }
                cx.notify();
            });
        }));
    }

    pub(crate) fn open_plan(
        &mut self,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.flush_open(cx);
        self.entry_menu = None;
        let Some(id) = id else {
            self.open = None;
            self.plan = None;
            cx.notify();
            return;
        };
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        self.open = Some((Section::Plans, id.clone()));
        self.plan = Some(PlanLoad::Loading);
        let project = self.project_id();
        let read = id.clone();
        let task = self
            .scope
            .run(async move { client.plan_document(&project, &read).await });
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.open != Some((Section::Plans, id.clone())) {
                    return;
                }
                this.plan = Some(match result {
                    Some(Ok(Some(document))) => {
                        this.plan_saved = document.raw.clone();
                        let raw = document.raw.clone();
                        this.plan_editor.update(cx, |input, cx| {
                            input.set_value(raw, window, cx);
                            input.focus(window, cx);
                        });
                        PlanLoad::Ready(Box::new(document))
                    }
                    Some(Ok(None)) => PlanLoad::Missing,
                    Some(Err(error)) => {
                        PlanLoad::Failed(error.message().unwrap_or_default().to_owned())
                    }
                    None => PlanLoad::Failed(String::new()),
                });
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn open_plan_id(&self) -> Option<&str> {
        match (&self.open, &self.plan) {
            (Some((Section::Plans, id)), Some(PlanLoad::Ready(_))) => Some(id),
            _ => None,
        }
    }

    fn schedule_plan_save(&mut self, cx: &mut Context<Self>) {
        if self.open_plan_id().is_none() {
            return;
        }
        self.plan_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PLAN_SAVE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.plan_timer = None;
                this.save_plan(cx);
            });
        }));
    }

    /// Writes the open plan's exact buffer. A plan gone meanwhile is shown as
    /// gone, never written back into existence.
    pub(crate) fn save_plan(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.open_plan_id().map(str::to_owned) else {
            return;
        };
        let raw = self.plan_editor.read(cx).value().to_string();
        if raw == self.plan_saved {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let previous = std::mem::replace(&mut self.plan_saved, raw.clone());
        self.plan_saving = true;
        let project = self.project_id();
        let task = self
            .scope
            .run(async move { client.save_plan(&project, &id, &raw).await });
        self.tasks.push(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.plan_saving = false;
                match result {
                    Some(Ok(Some((plan, _)))) => {
                        if let Some(knowledge) = &mut this.knowledge
                            && let Some(stored) = knowledge
                                .plans
                                .iter_mut()
                                .find(|stored| stored.id == plan.id)
                        {
                            *stored = plan.clone();
                        }
                        if let Some(PlanLoad::Ready(document)) = &mut this.plan {
                            document.plan = plan;
                        }
                    }
                    Some(Ok(None)) => this.plan = Some(PlanLoad::Missing),
                    _ => {
                        this.plan_saved = previous;
                        this.toast = Some(Toast {
                            error: true,
                            text: locale::strings(cx)
                                .get("rightSidebar.contextNotesTodo.toast.updatePlanFailed"),
                        });
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Implement or Improve: the open plan to a new session.
    pub(crate) fn send_plan(&mut self, implement: bool, cx: &mut Context<Self>) {
        if self.sending.is_some() {
            return;
        }
        let Some(id) = self.open_plan_id().map(str::to_owned) else {
            return;
        };
        // The send reads the file, so what is typed is saved first.
        self.plan_timer = None;
        self.save_plan(cx);
        self.sending = Some(id.clone());
        cx.emit(KnowledgeEvent::Dispatch(Dispatch::Plan { id, implement }));
        cx.notify();
    }

    /// The open plan's title, path and text, for its send.
    pub(crate) fn plan_send_parts(
        &self,
        id: &str,
        cx: &gpui_kit::App,
    ) -> Option<(String, Option<String>, String)> {
        let Some(PlanLoad::Ready(document)) = &self.plan else {
            return None;
        };
        (document.plan.id == id).then(|| {
            (
                document.plan.title.clone(),
                document.path.clone(),
                self.plan_editor.read(cx).value().to_string(),
            )
        })
    }

    /// The plan's card leads to the session it was sent to.
    pub(crate) fn plan_linked(&mut self, plan_id: &str, session_id: &str, cx: &mut Context<Self>) {
        if let Some(knowledge) = &mut self.knowledge
            && let Some(plan) = knowledge.plans.iter_mut().find(|plan| plan.id == plan_id)
        {
            plan.session_id = Some(session_id.to_owned());
        }
        cx.notify();
    }

    pub(crate) fn open_session(&mut self, session: &str, cx: &mut Context<Self>) {
        cx.emit(KnowledgeEvent::OpenSession(session.to_owned()));
    }

    pub(crate) fn toggle_entry_menu(&mut self, id: &str, cx: &mut Context<Self>) {
        self.entry_menu = (self.entry_menu.as_deref() != Some(id)).then(|| id.to_owned());
        self.todo_menu = None;
        cx.notify();
    }

    /// Moves a plan between the user's folder and the team's shared one.
    pub(crate) fn move_plan(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.entry_menu = None;
        if self.moving.is_some() {
            return;
        }
        let Some(share) = self
            .knowledge
            .as_ref()
            .and_then(|knowledge| knowledge.plans.iter().find(|plan| plan.id == id))
            .map(|plan| plan.source != knowledge::PlanSource::Shared)
        else {
            return;
        };
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        self.moving = Some(id.to_owned());
        let project = self.project_id();
        let plan = id.to_owned();
        let task = self
            .scope
            .run(async move { client.move_plan(&project, &plan, share).await });
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.moving = None;
                match result {
                    Some(Ok(Some((_, knowledge)))) => this.adopt(knowledge, cx),
                    Some(Ok(None)) => this.reload(cx),
                    _ => this.toast(
                        true,
                        "rightSidebar.contextNotesTodo.toast.movePlanFailed",
                        window,
                        cx,
                    ),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    // Deletes with undo.

    /// Hides a note or plan at once and deletes it when the undo window
    /// ends; Undo within the window keeps it, id and links intact.
    pub(crate) fn schedule_delete(
        &mut self,
        section: Section,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.entry_menu = None;
        if self.open.as_ref().is_some_and(|(_, open)| open == id) {
            self.note_timer = None;
            self.plan_timer = None;
            self.open = None;
            self.plan = None;
        }
        let key = id.to_owned();
        let timer = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(UNDO_WINDOW).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.run_delete(section, &key, window, cx);
            });
        });
        self.pending_deletes.insert(id.to_owned(), timer);
        let strings = locale::strings(cx).clone();
        let message = strings.get(if section == Section::Plans {
            "rightSidebar.contextNotesTodo.deleted.plan"
        } else {
            "rightSidebar.contextNotesTodo.deleted.note"
        });
        self.toast = Some(Toast {
            error: false,
            text: message.clone(),
        });
        let panel = cx.entity().downgrade();
        let undo = strings.get("rightSidebar.contextNotesTodo.undo");
        let id = id.to_owned();
        window.push_notification(
            Notification::info(message).action(move |_, _, _| {
                let panel = panel.clone();
                let id = id.clone();
                gpui_kit::component::button::Button::new("knowledge-undo-delete")
                    .label(undo.clone())
                    .on_click(move |_, _, cx| {
                        let _ = panel.update(cx, |panel, cx| panel.undo_delete(&id, cx));
                    })
            }),
            cx,
        );
        cx.notify();
    }

    pub(crate) fn undo_delete(&mut self, id: &str, cx: &mut Context<Self>) {
        self.pending_deletes.remove(id);
        cx.notify();
    }

    /// Sends a delete whose undo window ended; a failure brings it back.
    pub(crate) fn run_delete(
        &mut self,
        section: Section,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pending_deletes.remove(id).is_none() {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            self.toast(true, delete_failed_key(section), window, cx);
            return;
        };
        // Hidden until the server answers, so it does not flash back.
        self.pending_deletes.insert(id.to_owned(), Task::ready(()));
        let project = self.project_id();
        let entry = id.to_owned();
        let task = self.scope.run(async move {
            if section == Section::Plans {
                client.delete_plan(&project, &entry).await
            } else {
                client.delete_note(&project, &entry).await
            }
        });
        let id = id.to_owned();
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<Option<Knowledge>, KnowledgeError>> = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.pending_deletes.remove(&id);
                match result {
                    Some(Ok(Some(knowledge))) => this.adopt(knowledge, cx),
                    Some(Ok(None)) => this.reload(cx),
                    _ => this.toast(true, delete_failed_key(section), window, cx),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn delete_pending(&self, id: &str) -> bool {
        self.pending_deletes.contains_key(id)
    }

    /// The panel's own way back from an open entry.
    pub(crate) fn close_entry(&mut self, cx: &mut Context<Self>) {
        self.flush_open(cx);
        self.open = None;
        self.plan = None;
        cx.notify();
    }
}

fn delete_failed_key(section: Section) -> &'static str {
    if section == Section::Plans {
        "rightSidebar.contextNotesTodo.toast.deletePlanFailed"
    } else {
        "rightSidebar.contextNotesTodo.toast.deleteNoteFailed"
    }
}

/// A fresh id for a todo the panel shows before the server stores it.
fn new_id(prefix: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default(),
    );
    format!("{prefix}_{:016x}", hasher.finish())
}

/// A send's prompt, rendered once its overrides are read.
struct DispatchPrompt {
    visible: String,
    instructions: String,
}

impl crate::workspace::Workspace {
    /// The panel's owner for `directory`: the Chats root for a chat, else
    /// the project the folder or its worktree belongs to.
    pub(crate) fn knowledge_owner(
        &self,
        directory: &str,
        cx: &gpui_kit::App,
    ) -> Option<KnowledgeOwner> {
        if let Some(root) = self
            .topology
            .chat_roots
            .as_ref()
            .and_then(|roots| roots.root_of(Some(directory)))
        {
            return KnowledgeOwner::new(
                root,
                locale::strings(cx).get("sessions.scheduledTasks.dialog.project.chats"),
            );
        }
        let project = self.owning_project(directory)?.to_owned();
        KnowledgeOwner::new(&project, self.project_label(&project))
    }

    /// An owner's knowledge changed elsewhere: its open panels read again.
    pub(crate) fn project_context_changed(&mut self, project_id: &str, cx: &mut Context<Self>) {
        // A read asks the workspace for its connection, so it starts once
        // this update ends.
        for panel in self.panels.knowledge_views(&self.runtime) {
            let project = project_id.to_owned();
            let panel = panel.downgrade();
            cx.defer(move |cx| {
                let _ = panel.update(cx, |panel, cx| {
                    if panel.owner.project_id == project {
                        panel.reload(cx);
                    }
                });
            });
        }
    }

    pub(crate) fn on_knowledge_event(
        &mut self,
        panel: &Entity<KnowledgePanel>,
        event: &KnowledgeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            KnowledgeEvent::AppendToComposer(text) => {
                let taken = self.selected.is_some();
                if taken {
                    let current = self.composer.read(cx).value().to_string();
                    let next = knowledge::append_with_line_breaks(&current, text);
                    let end = next.len();
                    self.composer.update(cx, |composer, cx| {
                        composer.set_value(next, window, cx);
                        composer.set_selected_range(end..end, cx);
                        composer.focus(window, cx);
                    });
                    self.save_composer_draft(cx);
                }
                panel.update(cx, |panel, cx| panel.appended(taken, window, cx));
            }
            KnowledgeEvent::OpenSession(session) => {
                let id = openchamber_native_core::ids::SessionId::new(session.clone());
                self.open_session_by_id(id, window, cx);
            }
            KnowledgeEvent::Dispatch(dispatch) => {
                self.dispatch_knowledge(panel.clone(), dispatch.clone(), window, cx)
            }
        }
    }

    /// Starts a session in the panel's owner with a todo's or a plan's
    /// prompt, on the composer's model and agent, and opens it. A plan's
    /// card then leads to that session; a failed link costs only that.
    fn dispatch_knowledge(
        &mut self,
        panel: Entity<KnowledgePanel>,
        dispatch: Dispatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use openchamber_native_transport::prompt::Prompt;
        use openchamber_native_transport::prompt_parts::{PromptParts, SyntheticContext};
        use openchamber_native_transport::session_create::CreateSession;

        let owner = panel.read(cx).owner.clone();
        let fail = |panel: &Entity<KnowledgePanel>,
                    key: &'static str,
                    window: &mut Window,
                    cx: &mut Context<Self>| {
            panel.update(cx, |panel, cx| {
                panel.finish_dispatch(Some(key), true, window, cx)
            });
        };
        let Some(connection) = self.connection.as_ref() else {
            fail(
                &panel,
                "rightSidebar.contextNotesTodo.toast.createSessionFailed",
                window,
                cx,
            );
            return;
        };
        let Some(client) = connection.client.clone() else {
            fail(
                &panel,
                "rightSidebar.contextNotesTodo.toast.createSessionFailed",
                window,
                cx,
            );
            return;
        };
        let network = connection.network.handle();
        let selection = self.send_selection().map(|(_, selection)| selection);
        let model = selection
            .as_ref()
            .and_then(|selection| selection.model.clone())
            .filter(|model| !(model.provider_id == "openchamber" && model.model_id == "auto"));
        let agent = selection.and_then(|selection| selection.agent_name);
        let Ok(request) = CreateSession::new(
            self.session_ids.next(),
            &owner.path,
            model.clone(),
            agent.clone(),
        ) else {
            fail(
                &panel,
                "rightSidebar.contextNotesTodo.toast.createSessionFailed",
                window,
                cx,
            );
            return;
        };
        let (context_id, prompt_id) = (self.message_ids.next().0, self.message_ids.next().0);
        let plan_parts = match &dispatch {
            Dispatch::Plan { id, .. } => panel.read(cx).plan_send_parts(id, cx),
            Dispatch::Todo { .. } => None,
        };
        let directory = owner.path.clone();
        let project_id = owner.project_id.clone();
        let dispatch_for_task = dispatch.clone();
        let task = crate::network::NetworkTask::spawn_on(&network, async move {
            let overrides = client
                .magic_prompt_overrides(Some(&directory))
                .await
                .map_err(|_| "magicPrompts.toast.loadFailed")?;
            let render = |id: &str, variables: &[(&str, &str)]| {
                let variables = variables
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect();
                openchamber_native_core::magic_prompt::MagicPrompts {
                    overrides: &overrides,
                    defaults: crate::starters::MagicCatalog::get().templates(),
                }
                .render(id, &variables)
                .unwrap_or_default()
            };
            let (prompt, link) = match &dispatch_for_task {
                Dispatch::Todo { text, .. } => (
                    DispatchPrompt {
                        visible: render("plan.todo.visible", &[("todo_text", text)]),
                        instructions: render("plan.todo.instructions", &[("todo_text", text)]),
                    },
                    None,
                ),
                Dispatch::Plan { id, implement } => {
                    let (title, path, content) =
                        plan_parts.ok_or("rightSidebar.contextNotesTodo.toast.sendTodoFailed")?;
                    let action = if *implement {
                        "plan.implement"
                    } else {
                        "plan.improve"
                    };
                    let path_text = path.clone().unwrap_or_default();
                    let mut instructions = render(
                        &format!("{action}.instructions"),
                        &[("plan_title", &title), ("plan_path", &path_text)],
                    );
                    // Without a file to read, the plan's text rides along.
                    if path.is_none() {
                        instructions = [
                            instructions.as_str(),
                            "",
                            "The plan is not stored as a file in the repository and has no file path. Its full current contents follow below this note and are the source of truth for the plan. Where the instructions above refer to the plan file, treat the plan as stored in OpenChamber project knowledge (it is edited through the OpenChamber UI): propose plan revisions as plan text in the chat rather than editing a file.",
                            "",
                            content.as_str(),
                        ]
                        .join("\n");
                    }
                    (
                        DispatchPrompt {
                            visible: render(
                                &format!("{action}.visible"),
                                &[("plan_title", &title)],
                            ),
                            instructions,
                        },
                        Some(id.clone()),
                    )
                }
            };
            let record = client
                .create_session(&request)
                .await
                .map_err(|_| "rightSidebar.contextNotesTodo.toast.createSessionFailed")?;
            let linked = match &link {
                Some(plan) => client
                    .link_plan_session(&project_id, plan, record.id.as_str())
                    .await
                    .ok()
                    .flatten()
                    .map(|_| plan.clone()),
                None => None,
            };
            client
                .send_prompt(
                    &record.id,
                    &Prompt {
                        message_id: prompt_id,
                        text: prompt.visible,
                        parts: PromptParts {
                            context: vec![SyntheticContext {
                                id: context_id,
                                text: prompt.instructions,
                                description: None,
                                metadata: None,
                            }],
                            ..PromptParts::default()
                        },
                        model,
                        agent,
                        delivery: None,
                    },
                )
                .await
                .map_err(|_| "rightSidebar.contextNotesTodo.toast.sendTodoFailed")?;
            Ok::<_, &'static str>((record, linked))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task
                .await
                .unwrap_or(Err("rightSidebar.contextNotesTodo.toast.sendTodoFailed"));
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok((record, linked)) => {
                    let session = record.id.clone();
                    if let Some(plan) = linked {
                        panel.update(cx, |panel, cx| {
                            panel.plan_linked(&plan, session.as_str(), cx)
                        });
                    }
                    let key = matches!(dispatch, Dispatch::Todo { .. })
                        .then_some("rightSidebar.contextNotesTodo.toast.sentToNewSession");
                    panel.update(cx, |panel, cx| {
                        panel.finish_dispatch(key, false, window, cx)
                    });
                    if this.sessions.get(&session).is_none() {
                        this.apply_server_events(
                            vec![
                                openchamber_native_transport::event::ServerEvent::SessionCreated(
                                    record,
                                ),
                            ],
                            window,
                            cx,
                        );
                    }
                    this.select_session(session, window, cx);
                }
                Err(key) => {
                    panel.update(cx, |panel, cx| {
                        panel.finish_dispatch(Some(key), true, window, cx)
                    });
                }
            });
        })
        .detach();
    }
}
