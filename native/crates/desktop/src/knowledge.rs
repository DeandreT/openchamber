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
use gpui_kit::{
    AnyWindowHandle, AppContext, Context, Entity, EntityInputHandler, SharedString, Subscription,
    Task, Window,
};
use openchamber_native_core::ids::{ProjectId, RuntimeId};
use openchamber_native_core::knowledge::{
    self, Knowledge, NOTE_BODY_MAX_LENGTH, PLAN_TITLE_MAX_LENGTH, Section, TODO_TEXT_MAX_LENGTH,
    Todo,
};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::project_context::{KnowledgeError, PlanDocument, TodoChange};

use crate::files_view::PanelScope;
use crate::knowledge_writes::{
    DocumentDraft, DocumentKey, KnowledgeWriter, SaveOutcome, SaveReceipt,
};
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
    Plan {
        id: String,
        implement: bool,
        prompt: PlanPrompt,
    },
}

/// The plan document captured when Implement or Improve was pressed.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PlanPrompt {
    title: String,
    path: Option<String>,
    raw: String,
}

impl std::fmt::Debug for PlanPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanPrompt").finish_non_exhaustive()
    }
}

struct AcceptedPlan {
    dispatch: Dispatch,
    saved: Option<SaveReceipt>,
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

enum PendingDelete {
    Undo {
        _timer: Task<()>,
        client: ServerClient,
    },
    Writing,
}

pub(crate) struct KnowledgePanel {
    window: AnyWindowHandle,
    pub(crate) scope: PanelScope,
    pub(crate) owner: KnowledgeOwner,
    /// The last good snapshot; `None` until one loaded.
    pub(crate) knowledge: Option<Knowledge>,
    pub(crate) loading: bool,
    pub(crate) load_error: Option<String>,
    load_reported: bool,
    load_revision: u64,
    mutation_revision: u64,
    pending_mutations: usize,
    refresh_after_mutations: bool,
    load_task: Option<Task<()>>,
    last_read: Option<Instant>,
    pub(crate) section: Section,
    pub(crate) search_open: bool,
    pub(crate) search: Entity<InputState>,
    pub(crate) query: String,
    pub(crate) open: Option<(Section, String)>,
    pub(crate) note_input: Entity<TextareaState>,
    pub(crate) creating_note: bool,
    failed_note_creation: bool,
    pub(crate) todo_input: Entity<InputState>,
    pub(crate) plan_input: Entity<InputState>,
    pub(crate) creating_plan: bool,
    failed_plan_creation: bool,
    pub(crate) note_editor: Entity<TextareaState>,
    note_timer: Option<Task<()>>,
    adopted_note: Option<String>,
    pub(crate) plan: Option<PlanLoad>,
    pub(crate) plan_editor: Entity<TextareaState>,
    plan_timer: Option<Task<()>>,
    plan_read_revision: u64,
    documents: HashMap<DocumentKey, DocumentDraft>,
    writer: Option<KnowledgeWriter>,
    pending_plan_send: Option<AcceptedPlan>,
    pending_dispatches: usize,
    pub(crate) closing: bool,
    flush_waiters: Vec<tokio::sync::oneshot::Sender<()>>,
    /// Deletes waiting out their undo window, by entry id.
    pending_deletes: HashMap<String, PendingDelete>,
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
        let note_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .validate(|text, _| text.encode_utf16().count() <= NOTE_BODY_MAX_LENGTH)
        });
        let todo_input = cx.new(|cx| InputState::new(window, cx));
        let plan_input = cx.new(|cx| InputState::new(window, cx));
        let note_editor = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(12, 40)
                .validate(|text, _| text.encode_utf16().count() <= NOTE_BODY_MAX_LENGTH)
        });
        let plan_editor = cx.new(|cx| TextareaState::new(window, cx).auto_grow(16, 60));
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, input, event, _, cx| {
                if let InputEvent::Change = event {
                    this.set_query(input.read(cx).value().to_string(), cx);
                }
            }),
            cx.subscribe_in(&note_input, window, |this, input, event, window, cx| {
                if matches!(event, InputEvent::Change) && input.read(cx).value().is_empty() {
                    this.failed_note_creation = false;
                }
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
            cx.subscribe_in(&plan_input, window, |this, input, event, window, cx| {
                if matches!(event, InputEvent::Change) && input.read(cx).value().is_empty() {
                    this.failed_plan_creation = false;
                }
                if let InputEvent::PressEnter { .. } = event {
                    this.add_plan(window, cx);
                }
            }),
            cx.subscribe_in(&note_editor, window, |this, input, event, window, cx| {
                if matches!(event, InputEvent::Change)
                    && input.update(cx, |input, cx| {
                        input.marked_text_range(window, cx).is_none()
                    })
                {
                    this.schedule_note_save(cx);
                }
            }),
            cx.subscribe_in(&plan_editor, window, |this, input, event, window, cx| {
                if matches!(event, InputEvent::Change)
                    && input.update(cx, |input, cx| {
                        input.marked_text_range(window, cx).is_none()
                    })
                {
                    this.schedule_plan_save(cx);
                }
            }),
        ];
        let panel = Self {
            window: window.window_handle(),
            scope,
            owner,
            knowledge: None,
            loading: false,
            load_error: None,
            load_reported: false,
            load_revision: 0,
            mutation_revision: 0,
            pending_mutations: 0,
            refresh_after_mutations: false,
            load_task: None,
            last_read: None,
            section: Section::Notes,
            search_open: false,
            search,
            query: String::new(),
            open: None,
            note_input,
            creating_note: false,
            failed_note_creation: false,
            todo_input,
            plan_input,
            creating_plan: false,
            failed_plan_creation: false,
            note_editor,
            note_timer: None,
            adopted_note: None,
            plan: None,
            plan_editor,
            plan_timer: None,
            plan_read_revision: 0,
            documents: HashMap::new(),
            writer: None,
            pending_plan_send: None,
            pending_dispatches: 0,
            closing: false,
            flush_waiters: Vec::new(),
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
            let _ = this.update(cx, |this, cx| this.finish_load(revision, result, cx));
        }));
        cx.notify();
    }

    fn finish_load(
        &mut self,
        revision: u64,
        result: Result<Knowledge, KnowledgeError>,
        cx: &mut Context<Self>,
    ) {
        if self.load_revision != revision {
            return;
        }
        self.loading = false;
        if self.pending_mutations > 0 {
            self.refresh_after_mutations = true;
            cx.notify();
            return;
        }
        match result {
            Ok(knowledge) => {
                self.load_error = None;
                self.load_reported = false;
                self.adopt(knowledge, cx);
            }
            Err(error) => {
                self.load_error = Some(error.message().unwrap_or_default().to_owned());
            }
        }
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
        // A confirmed mutation also supersedes any read started before it.
        self.load_revision += 1;
        self.loading = false;
        for (key, draft) in &mut self.documents {
            let present = match key {
                DocumentKey::Note(id) => knowledge.notes.iter().any(|note| note.id == *id),
                DocumentKey::Plan(id) => knowledge.plans.iter().any(|plan| plan.id == *id),
            };
            if !present && draft.dirty() {
                draft.missing = true;
            }
        }
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

    fn begin_mutation(&mut self) -> u64 {
        self.load_revision += 1;
        self.loading = false;
        self.refresh_after_mutations |= self.pending_mutations > 0;
        self.pending_mutations += 1;
        self.mutation_revision += 1;
        self.mutation_revision
    }

    fn ensure_writer(&mut self) -> bool {
        if self.writer.is_none() {
            let Ok(writer) = KnowledgeWriter::new() else {
                return false;
            };
            self.writer = Some(writer);
        }
        true
    }

    fn mutation_client(&mut self, cx: &mut Context<Self>) -> Option<ServerClient> {
        if self.closing {
            return None;
        }
        let client = self.scope.client(cx)?;
        self.ensure_writer().then_some(client)
    }

    fn finish_mutation(
        &mut self,
        revision: u64,
        snapshot: Option<Knowledge>,
        failed: bool,
        cx: &mut Context<Self>,
    ) {
        self.pending_mutations -= 1;
        self.load_revision += 1;
        self.loading = false;
        self.refresh_after_mutations |= failed || revision != self.mutation_revision;
        if self.pending_mutations == 0 {
            if std::mem::take(&mut self.refresh_after_mutations) {
                self.reload(cx);
            } else if let Some(snapshot) = snapshot {
                self.adopt(snapshot, cx);
            }
        }
        if self.is_flushed() {
            for waiting in self.flush_waiters.drain(..) {
                let _ = waiting.send(());
            }
        }
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
        if self.closing || self.creating_note {
            return;
        }
        let body = self.note_input.read(cx).value().to_string();
        if body.trim().is_empty() || body.encode_utf16().count() > NOTE_BODY_MAX_LENGTH {
            return;
        }
        let Some(client) = self.mutation_client(cx) else {
            return;
        };
        let revision = self.begin_mutation();
        self.creating_note = true;
        self.failed_note_creation = false;
        // The field clears at once and stays usable; a failure puts the text
        // back if nothing new was typed meanwhile.
        self.note_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        let project = self.project_id();
        let sent = body.clone();
        let task = self
            .writer
            .as_ref()
            .expect("a mutation client initialized the writer")
            .run(async move { client.create_note(&project, &sent).await });
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.creating_note = false;
                match result {
                    Some(Ok((note, knowledge))) => {
                        if let Some(current) = &mut this.knowledge
                            && !current.notes.iter().any(|stored| stored.id == note.id)
                        {
                            current.notes.push(note);
                        }
                        this.finish_mutation(revision, Some(knowledge), false, cx);
                    }
                    _ => {
                        this.failed_note_creation = true;
                        this.finish_mutation(revision, None, true, cx);
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
        let key = DocumentKey::Note(id.clone());
        let draft = self
            .documents
            .entry(key)
            .or_insert_with(|| DocumentDraft::new(body.clone()));
        if !draft.dirty() && draft.pending.is_none() {
            draft.saved.clone_from(&body);
            draft.set_text(body);
        }
        let body = draft.text.clone();
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
        let key = DocumentKey::Note(id);
        let Some(buffer) = self.documents.get_mut(&key) else {
            return;
        };
        if buffer.pending.is_some() || buffer.dirty() || draft != buffer.saved {
            return;
        }
        buffer.saved.clone_from(&stored);
        buffer.set_text(stored.clone());
        // Setting an input's value needs the window; the next frame applies it.
        self.adopted_note = Some(stored);
        cx.notify();
    }

    /// The stored body an untouched open note takes on its next frame.
    pub(crate) fn take_adopted_note(&mut self) -> Option<String> {
        self.adopted_note.take()
    }

    fn schedule_note_save(&mut self, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        let Some(key) = self.capture_note(cx) else {
            return;
        };
        self.note_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTE_SAVE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.note_timer = None;
                this.save_document(key, cx);
            });
        }));
    }

    /// Saves the open note's draft. A blank body is never sent: the server
    /// rejects it, and deleting is its own action.
    #[cfg(test)]
    pub(crate) fn save_note(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.capture_note(cx) {
            self.save_document(key, cx);
        }
    }

    /// Leaving an entry saves what is still waiting for its pause.
    fn flush_open(&mut self, cx: &mut Context<Self>) {
        self.note_timer = None;
        self.plan_timer = None;
        if !self.closing
            && let Some(client) = self.scope.client(cx)
        {
            self.refresh_write_client(&self.scope.runtime.clone(), &client);
        }
        // Change events capture committed text. Navigation must not capture
        // provisional IME text directly from the editor.
        if let Some(id) = self.open_note_id() {
            self.save_document(DocumentKey::Note(id.to_owned()), cx);
        }
        if let Some(id) = self.open_plan_id() {
            self.save_document(DocumentKey::Plan(id.to_owned()), cx);
        }
    }

    /// Refreshes retry credentials only for this panel's original runtime.
    pub(crate) fn refresh_write_client(&mut self, runtime: &RuntimeId, client: &ServerClient) {
        if &self.scope.runtime != runtime {
            return;
        }
        for draft in self.documents.values_mut() {
            draft.client = Some(client.clone());
        }
    }

    fn capture_note(&mut self, cx: &mut Context<Self>) -> Option<DocumentKey> {
        let id = self.open_note_id()?.to_owned();
        let key = DocumentKey::Note(id.clone());
        let saved = self
            .knowledge
            .as_ref()?
            .notes
            .iter()
            .find(|note| note.id == id)?
            .body
            .clone();
        let client = if self.closing {
            None
        } else {
            self.scope.client(cx)
        };
        let draft = self
            .documents
            .entry(key.clone())
            .or_insert_with(|| DocumentDraft::new(saved));
        draft.set_text(self.note_editor.read(cx).value().to_string());
        if client.is_some() {
            draft.client = client;
        }
        Some(key)
    }

    fn capture_plan(&mut self, cx: &mut Context<Self>) -> Option<DocumentKey> {
        let id = self.open_plan_id()?.to_owned();
        let Some(PlanLoad::Ready(document)) = &self.plan else {
            return None;
        };
        let saved = document.raw.clone();
        let key = DocumentKey::Plan(id);
        let client = if self.closing {
            None
        } else {
            self.scope.client(cx)
        };
        let draft = self
            .documents
            .entry(key.clone())
            .or_insert_with(|| DocumentDraft::new(saved));
        draft.set_text(self.plan_editor.read(cx).value().to_string());
        if client.is_some() {
            draft.client = client;
        }
        Some(key)
    }

    fn save_failure(&mut self, key: &DocumentKey, cx: &mut Context<Self>) {
        let text = locale::strings(cx).get(match key {
            DocumentKey::Note(_) => "rightSidebar.contextNotesTodo.toast.saveNotesFailed",
            DocumentKey::Plan(_) => "rightSidebar.contextNotesTodo.toast.updatePlanFailed",
        });
        self.toast = Some(Toast {
            error: true,
            text: text.clone(),
        });
        let window = self.window;
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                window.push_notification(Notification::error(text), cx);
            });
        });
        cx.notify();
    }

    fn save_document(&mut self, key: DocumentKey, cx: &mut Context<Self>) {
        let Some(draft) = self.documents.get(&key) else {
            return;
        };
        if draft.pending.is_some() || !draft.dirty() || draft.missing {
            return;
        }
        if matches!(key, DocumentKey::Note(_)) && draft.text.trim().is_empty() {
            self.save_failure(&key, cx);
            return;
        }
        if draft.client.is_none() {
            self.save_failure(&key, cx);
            return;
        }
        if !self.ensure_writer() {
            self.save_failure(&key, cx);
            return;
        }
        let Some(sent) = self.documents.get_mut(&key).and_then(DocumentDraft::begin) else {
            return;
        };
        self.queue_document_save(key, sent, cx);
    }

    /// A newer captured plan can queue behind its older write before its send.
    fn queue_document_save(
        &mut self,
        key: DocumentKey,
        sent: String,
        cx: &mut Context<Self>,
    ) -> SaveReceipt {
        let client = self.documents[&key]
            .client
            .clone()
            .expect("an accepted save captured its client");
        let revision = self.begin_mutation();
        let receipt = self
            .writer
            .as_ref()
            .expect("the writer was initialized")
            .save_observed(client, self.project_id(), key.clone(), sent.clone());
        let draft = self
            .documents
            .get_mut(&key)
            .expect("the captured document exists");
        if draft.pending.as_deref() != Some(sent.as_str()) {
            draft.revision += 1;
        }
        draft.pending = Some(sent.clone());
        draft.pending_save = Some(receipt.clone());
        let acknowledgment = receipt.clone();
        self.tasks.push(cx.spawn(async move |this, cx| {
            let outcome = acknowledgment.clone().outcome().await;
            let _ = this.update(cx, |this, cx| {
                if let Some(draft) = this.documents.get_mut(&key) {
                    draft.acknowledge_save(revision, &sent, &outcome);
                }
                if this
                    .documents
                    .get(&key)
                    .and_then(|draft| draft.pending_save.as_ref())
                    .is_none_or(|pending| !pending.same_save(&acknowledgment))
                {
                    this.finish_mutation(revision, None, false, cx);
                    cx.notify();
                    return;
                }
                this.finish_document(revision, key, sent, outcome, cx)
            });
        }));
        cx.notify();
        receipt
    }

    fn finish_document(
        &mut self,
        revision: u64,
        key: DocumentKey,
        sent: String,
        outcome: SaveOutcome,
        cx: &mut Context<Self>,
    ) {
        if self
            .documents
            .get(&key)
            .is_none_or(|draft| draft.pending.as_deref() != Some(sent.as_str()))
        {
            self.finish_mutation(revision, None, false, cx);
            cx.notify();
            return;
        }
        let follow = self
            .documents
            .get_mut(&key)
            .is_some_and(|draft| draft.finish(&sent, &outcome));
        let failed = !outcome.succeeded();
        match outcome {
            SaveOutcome::Note(note) => {
                if let Some(knowledge) = &mut self.knowledge
                    && let Some(stored) = knowledge
                        .notes
                        .iter_mut()
                        .find(|stored| stored.id == note.id)
                {
                    *stored = note;
                }
            }
            SaveOutcome::Plan(plan) => {
                if let Some(knowledge) = &mut self.knowledge
                    && let Some(stored) = knowledge
                        .plans
                        .iter_mut()
                        .find(|stored| stored.id == plan.id)
                {
                    *stored = plan.clone();
                }
                if let Some(PlanLoad::Ready(document)) = &mut self.plan
                    && document.plan.id == plan.id
                {
                    document.plan = plan;
                    document.raw = sent;
                }
            }
            SaveOutcome::Missing => {
                if matches!(&key, DocumentKey::Plan(id) if self.open_plan_id() == Some(id.as_str()))
                {
                    self.plan = Some(PlanLoad::Missing);
                }
                self.save_failure(&key, cx);
            }
            SaveOutcome::Failed => self.save_failure(&key, cx),
        }
        if follow {
            self.save_document(key, cx);
        }
        self.finish_mutation(revision, None, failed, cx);
        cx.notify();
    }

    /// Returns the exact captured plan's save, already ahead of its send on FIFO.
    fn dispatch_saved_plan(
        &mut self,
        dispatch: &Dispatch,
        cx: &mut Context<Self>,
    ) -> Result<Option<SaveReceipt>, ()> {
        let Dispatch::Plan { id, prompt, .. } = dispatch else {
            return Ok(None);
        };
        let key = DocumentKey::Plan(id.clone());
        let Some(draft) = self.documents.get(&key) else {
            return Err(());
        };
        if draft.missing {
            return Err(());
        }
        if draft.pending.as_deref() == Some(prompt.raw.as_str()) {
            return draft.pending_save.clone().map(Some).ok_or(());
        }
        if draft.pending.is_none() && draft.saved == prompt.raw {
            return Ok(None);
        }
        if draft.client.is_none() || !self.ensure_writer() {
            return Err(());
        }
        Ok(Some(self.queue_document_save(key, prompt.raw.clone(), cx)))
    }

    fn finish_dispatch_io(&mut self, cx: &mut Context<Self>) {
        self.pending_dispatches -= 1;
        if self.is_flushed() {
            for waiting in self.flush_waiters.drain(..) {
                let _ = waiting.send(());
            }
        }
        cx.notify();
    }

    /// Captures editor values and starts every retained dirty entry's write.
    pub(crate) fn prepare_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.note_timer = None;
        self.plan_timer = None;
        self.pending_deletes
            .retain(|_, pending| matches!(pending, PendingDelete::Writing));
        if self.note_editor.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_none()
        }) {
            let _ = self.capture_note(cx);
        }
        if self.plan_editor.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_none()
        }) {
            let _ = self.capture_plan(cx);
        }
        let keys: Vec<_> = self.documents.keys().cloned().collect();
        for key in keys {
            self.save_document(key, cx);
        }
    }

    pub(crate) fn is_flushed(&self) -> bool {
        self.pending_dispatches == 0
            && self.pending_mutations == 0
            && self.documents.values().all(|draft| draft.pending.is_none())
    }

    pub(crate) fn has_failed_writes(&self) -> bool {
        self.failed_note_creation
            || self.failed_plan_creation
            || self
                .documents
                .values()
                .any(|draft| draft.dirty() && draft.pending.is_none())
    }

    pub(crate) fn take_pending_write(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        self.closing = true;
        cx.notify();
        if self.is_flushed() {
            return None;
        }
        let (ready, waiting) = tokio::sync::oneshot::channel();
        self.flush_waiters.push(ready);
        Some(waiting)
    }

    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.closing = false;
        cx.notify();
    }

    pub(crate) fn take_quit_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.closing = true;
        self.note_timer = None;
        self.plan_timer = None;
        self.pending_deletes
            .retain(|_, pending| matches!(pending, PendingDelete::Writing));
        self.flush_waiters.clear();
        let writer = self.writer.take()?;
        // A later edit may be waiting on its entry's current foreground callback.
        for (key, draft) in &self.documents {
            if draft
                .pending
                .as_ref()
                .is_some_and(|pending| pending != &draft.text)
                && let Some(client) = &draft.client
            {
                drop(writer.save(
                    client.clone(),
                    self.project_id(),
                    key.clone(),
                    draft.text.clone(),
                ));
            }
        }
        Some(cx.background_executor().spawn(writer.finish()))
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
        if self.closing {
            return;
        }
        let Some(client) = self.mutation_client(cx) else {
            return;
        };
        let revision = self.begin_mutation();
        if let Some(knowledge) = &mut self.knowledge {
            knowledge.todos = next;
        }
        self.todo_writes += 1;
        let project = self.project_id();
        let task = self
            .writer
            .as_ref()
            .expect("a mutation client initialized the writer")
            .run(async move { client.change_todo(&project, &change).await });
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.todo_writes -= 1;
                match result {
                    Some(Ok(Some(knowledge))) => {
                        this.finish_mutation(revision, Some(knowledge), false, cx)
                    }
                    Some(Ok(None)) => this.finish_mutation(revision, None, true, cx),
                    _ => {
                        this.finish_mutation(revision, None, true, cx);
                        this.toast(
                            true,
                            "rightSidebar.contextNotesTodo.toast.saveNotesFailed",
                            window,
                            cx,
                        );
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn add_todo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mutation_client(cx).is_none() {
            return;
        }
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
        if self.closing {
            return;
        }
        let todos = self.todos();
        let next: Vec<Todo> = todos
            .iter()
            .filter(|todo| !todo.completed)
            .cloned()
            .collect();
        if next.len() == todos.len() {
            return;
        }
        let Some(client) = self.mutation_client(cx) else {
            return;
        };
        let revision = self.begin_mutation();
        if let Some(knowledge) = &mut self.knowledge {
            knowledge.todos = next.clone();
        }
        let project = self.project_id();
        let task = self
            .writer
            .as_ref()
            .expect("a mutation client initialized the writer")
            .run(async move { client.save_todos(&project, &next).await });
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Some(Ok(knowledge)) => {
                        this.finish_mutation(revision, Some(knowledge), false, cx)
                    }
                    _ => {
                        this.finish_mutation(revision, None, true, cx);
                        this.toast(
                            true,
                            "rightSidebar.contextNotesTodo.toast.saveNotesFailed",
                            window,
                            cx,
                        );
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
        if self.closing {
            return;
        }
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
        if self.sending.is_some() || self.closing {
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
        self.pending_plan_send = None;
        if let Some(key) = key {
            self.toast(error, key, window, cx);
        }
        cx.notify();
    }

    // Plans.

    pub(crate) fn add_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || self.creating_plan {
            return;
        }
        let title =
            knowledge::single_line(&self.plan_input.read(cx).value(), PLAN_TITLE_MAX_LENGTH);
        let title = title.trim().to_owned();
        if title.is_empty() {
            return;
        }
        let Some(client) = self.mutation_client(cx) else {
            return;
        };
        let revision = self.begin_mutation();
        self.creating_plan = true;
        self.failed_plan_creation = false;
        self.plan_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        let project = self.project_id();
        let sent = title.clone();
        let task = self
            .writer
            .as_ref()
            .expect("a mutation client initialized the writer")
            .run(async move { client.create_plan(&project, &sent, "").await });
        // Plan creation is not optimistic: the id and file come from the
        // server, and the new plan opens once it exists.
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.creating_plan = false;
                match result {
                    Some(Ok((plan, knowledge))) => {
                        if let Some(current) = &mut this.knowledge
                            && !current.plans.iter().any(|stored| stored.id == plan.id)
                        {
                            current.plans.push(plan.clone());
                        }
                        this.finish_mutation(revision, Some(knowledge), false, cx);
                        if this.section == Section::Plans && !this.closing {
                            this.open_plan(Some(plan.id), window, cx);
                        }
                    }
                    _ => {
                        this.failed_plan_creation = true;
                        this.finish_mutation(revision, None, true, cx);
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
        self.plan_read_revision += 1;
        let revision = self.plan_read_revision;
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
        let document_revision = self
            .documents
            .get(&DocumentKey::Plan(id.clone()))
            .map(|draft| draft.revision);
        let project = self.project_id();
        let read = id.clone();
        let task = self
            .scope
            .run(async move { client.plan_document(&project, &read).await });
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.open != Some((Section::Plans, id.clone()))
                    || this.plan_read_revision != revision
                {
                    return;
                }
                this.plan = Some(match result {
                    Some(Ok(Some(document))) => {
                        let key = DocumentKey::Plan(id.clone());
                        let draft = this
                            .documents
                            .entry(key)
                            .and_modify(|draft| {
                                if Some(draft.revision) == document_revision
                                    && !draft.dirty()
                                    && draft.pending.is_none()
                                {
                                    draft.saved.clone_from(&document.raw);
                                    draft.set_text(document.raw.clone());
                                }
                            })
                            .or_insert_with(|| DocumentDraft::new(document.raw.clone()));
                        let raw = draft.text.clone();
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
        if self.closing {
            return;
        }
        let Some(key) = self.capture_plan(cx) else {
            return;
        };
        self.plan_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PLAN_SAVE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.plan_timer = None;
                this.save_document(key, cx);
            });
        }));
    }

    /// Writes the open plan's exact buffer. A plan gone meanwhile is shown as
    /// gone, never written back into existence.
    pub(crate) fn save_plan(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.capture_plan(cx) {
            self.save_document(key, cx);
        }
    }

    /// Implement or Improve: the open plan to a new session.
    pub(crate) fn send_plan(&mut self, implement: bool, cx: &mut Context<Self>) {
        if self.sending.is_some() || self.closing {
            return;
        }
        let Some(id) = self.open_plan_id().map(str::to_owned) else {
            return;
        };
        let Some((title, path, raw)) = self.plan_send_parts(&id, cx) else {
            return;
        };
        // The send reads the file, so what is typed is saved first.
        self.plan_timer = None;
        self.sending = Some(id.clone());
        let dispatch = Dispatch::Plan {
            id: id.clone(),
            implement,
            prompt: PlanPrompt { title, path, raw },
        };
        self.save_plan(cx);
        let Ok(saved) = self.dispatch_saved_plan(&dispatch, cx) else {
            self.sending = None;
            self.save_failure(&DocumentKey::Plan(id), cx);
            return;
        };
        self.pending_plan_send = Some(AcceptedPlan {
            dispatch: dispatch.clone(),
            saved,
        });
        // Acceptance captures the workspace configuration now. Its independent
        // writer waits for persistence before creating or sending anything.
        cx.emit(KnowledgeEvent::Dispatch(dispatch));
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
        if self.moving.is_some() || self.closing {
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
        let Some(client) = self.mutation_client(cx) else {
            return;
        };
        let revision = self.begin_mutation();
        self.moving = Some(id.to_owned());
        let project = self.project_id();
        let plan = id.to_owned();
        let task = self
            .writer
            .as_ref()
            .expect("a mutation client initialized the writer")
            .run(async move { client.move_plan(&project, &plan, share).await });
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.moving = None;
                match result {
                    Some(Ok(Some((_, knowledge)))) => {
                        this.finish_mutation(revision, Some(knowledge), false, cx)
                    }
                    Some(Ok(None)) => this.finish_mutation(revision, None, true, cx),
                    _ => {
                        this.finish_mutation(revision, None, true, cx);
                        this.toast(
                            true,
                            "rightSidebar.contextNotesTodo.toast.movePlanFailed",
                            window,
                            cx,
                        );
                    }
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
        if self.closing || self.pending_deletes.contains_key(id) {
            return;
        }
        let Some(client) = self.mutation_client(cx) else {
            self.toast(true, delete_failed_key(section), window, cx);
            return;
        };
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
        self.pending_deletes.insert(
            id.to_owned(),
            PendingDelete::Undo {
                _timer: timer,
                client,
            },
        );
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
        if matches!(
            self.pending_deletes.get(id),
            Some(PendingDelete::Undo { .. })
        ) {
            self.pending_deletes.remove(id);
        }
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
        let Some(pending) = self.pending_deletes.get_mut(id) else {
            return;
        };
        let PendingDelete::Undo { client, .. } = std::mem::replace(pending, PendingDelete::Writing)
        else {
            return;
        };
        let revision = self.begin_mutation();
        // Hidden until the server answers, so it does not flash back.
        let project = self.project_id();
        let entry = id.to_owned();
        let task = self
            .writer
            .as_ref()
            .expect("a mutation client initialized the writer")
            .run(async move {
                if section == Section::Plans {
                    client.delete_plan(&project, &entry).await
                } else {
                    client.delete_note(&project, &entry).await
                }
            });
        let id = id.to_owned();
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<Option<Knowledge>, KnowledgeError>> = task.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.pending_deletes.remove(&id);
                match result {
                    Some(Ok(snapshot)) => {
                        if let Some(current) = &mut this.knowledge {
                            if section == Section::Notes {
                                current.notes.retain(|note| note.id != id);
                                this.documents.remove(&DocumentKey::Note(id.clone()));
                            } else {
                                current.plans.retain(|plan| plan.id != id);
                                this.documents.remove(&DocumentKey::Plan(id.clone()));
                            }
                        }
                        this.finish_mutation(revision, snapshot, false, cx);
                    }
                    _ => {
                        this.finish_mutation(revision, None, true, cx);
                        this.toast(true, delete_failed_key(section), window, cx);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn delete_pending(&self, id: &str) -> bool {
        self.pending_deletes.contains_key(id)
    }

    pub(crate) fn deleted_drafts(&self) -> Vec<(&DocumentKey, &str)> {
        let mut drafts: Vec<_> = self
            .documents
            .iter()
            .filter(|(_, draft)| draft.missing && draft.dirty() && draft.pending.is_none())
            .map(|(key, draft)| (key, draft.text.as_str()))
            .collect();
        drafts.sort_by(|(left, _), (right, _)| match (left, right) {
            (DocumentKey::Note(left), DocumentKey::Note(right))
            | (DocumentKey::Plan(left), DocumentKey::Plan(right)) => left.cmp(right),
            (DocumentKey::Note(_), DocumentKey::Plan(_)) => std::cmp::Ordering::Less,
            (DocumentKey::Plan(_), DocumentKey::Note(_)) => std::cmp::Ordering::Greater,
        });
        drafts
    }

    pub(crate) fn copy_deleted_draft(&self, key: &DocumentKey, cx: &mut Context<Self>) {
        if let Some(draft) = self.documents.get(key)
            && draft.missing
        {
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(draft.text.clone()));
        }
    }

    pub(crate) fn discard_deleted_draft(&mut self, key: &DocumentKey, cx: &mut Context<Self>) {
        if self.closing
            || self
                .documents
                .get(key)
                .is_none_or(|draft| !draft.missing || draft.pending.is_some())
        {
            return;
        }
        self.documents.remove(key);
        let entry = match key {
            DocumentKey::Note(id) => (Section::Notes, id.clone()),
            DocumentKey::Plan(id) => (Section::Plans, id.clone()),
        };
        if self.open.as_ref() == Some(&entry) {
            self.open = None;
            self.plan = None;
        }
        cx.notify();
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

#[cfg(test)]
#[path = "knowledge_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "knowledge_dispatch_tests.rs"]
mod dispatch_tests;

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
        if self.close_in_progress() || panel.read(cx).scope.runtime != self.runtime {
            if matches!(event, KnowledgeEvent::Dispatch(_)) {
                panel.update(cx, |panel, cx| {
                    panel.finish_dispatch(None, false, window, cx)
                });
            }
            return;
        }
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
        if panel.read(cx).scope.runtime != self.runtime {
            fail(
                &panel,
                "rightSidebar.contextNotesTodo.toast.createSessionFailed",
                window,
                cx,
            );
            return;
        }
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
        let runtime = self.runtime.clone();
        let generation = connection.generation;
        let navigation = self.navigation_revision;
        let selection = self.send_selection().map(|(_, selection)| selection);
        let model = selection
            .as_ref()
            .and_then(|selection| selection.model.clone());
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
        let directory = owner.path.clone();
        let project_id = owner.project_id.clone();
        let prerequisite = panel.update(cx, |panel, _| {
            if !panel.ensure_writer() {
                return Err(());
            }
            match &dispatch {
                Dispatch::Todo { .. } => Ok(None),
                Dispatch::Plan { .. } => panel
                    .pending_plan_send
                    .take()
                    .filter(|accepted| accepted.dispatch == dispatch)
                    .map(|accepted| accepted.saved)
                    .ok_or(()),
            }
        });
        let Ok(prerequisite) = prerequisite else {
            fail(
                &panel,
                "rightSidebar.contextNotesTodo.toast.updatePlanFailed",
                window,
                cx,
            );
            return;
        };
        let dispatch_for_task = dispatch.clone();
        let operation = async move {
            if let Some(prerequisite) = prerequisite
                && !prerequisite.outcome().await.succeeded()
            {
                return Err("rightSidebar.contextNotesTodo.toast.updatePlanFailed");
            }
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
                Dispatch::Plan {
                    id,
                    implement,
                    prompt,
                } => {
                    let PlanPrompt {
                        title,
                        path,
                        raw: content,
                    } = prompt;
                    let action = if *implement {
                        "plan.implement"
                    } else {
                        "plan.improve"
                    };
                    let path_text = path.clone().unwrap_or_default();
                    let mut instructions = render(
                        &format!("{action}.instructions"),
                        &[("plan_title", title), ("plan_path", &path_text)],
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
                            visible: render(&format!("{action}.visible"), &[("plan_title", title)]),
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
        };
        let task = panel.update(cx, |panel, _| {
            panel.pending_dispatches += 1;
            panel
                .writer
                .as_ref()
                .expect("acceptance initialized the writer")
                .run(operation)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task
                .await
                .unwrap_or(Err("rightSidebar.contextNotesTodo.toast.sendTodoFailed"));
            let _ = this.update_in(cx, |this, window, cx| {
                panel.update(cx, |panel, cx| panel.finish_dispatch_io(cx));
                if this.close_in_progress()
                    || this.runtime != runtime
                    || this.navigation_revision != navigation
                    || this
                        .connection
                        .as_ref()
                        .is_none_or(|connection| connection.generation != generation)
                {
                    panel.update(cx, |panel, cx| {
                        panel.finish_dispatch(None, false, window, cx)
                    });
                    return;
                }
                match result {
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
                }
            });
        })
        .detach();
    }
}
