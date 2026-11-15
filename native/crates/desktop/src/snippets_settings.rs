//! Settings → Snippets (`SnippetsSidebar.tsx`, `SnippetsPage.tsx`): the
//! snippet list grouped by where each file lives, and one snippet's editor.
//! This page saves explicitly; a switch away from unsaved edits asks first.

use std::collections::HashMap;

use gpui_kit::component::IndexPath;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Theme, WindowExt};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, Toggled, WeakEntity,
    Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_transport::snippets::{
    NameProblem, Snippet, SnippetError, SnippetFields, SnippetScope, is_valid_snippet_name,
    normalize_snippet_name_input, snippet_name_problem, unique_snippet_name,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::network::{Network, NetworkTask};
use crate::settings_controls::{
    Load, ServerTarget, SettingsWrites, confirm, focus_handle, helper, load_failed, same_target,
};
use crate::settings_navigation::Page;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The editor's fields, compared whole to tell an edit from its baseline.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Form {
    name: String,
    scope: SnippetScope,
    description: String,
    aliases: Vec<String>,
    content: String,
}

impl Form {
    fn of(snippet: &Snippet) -> Self {
        Self {
            name: snippet.name.clone(),
            scope: snippet.source,
            description: snippet.description.clone().unwrap_or_default(),
            aliases: snippet.aliases.clone(),
            content: snippet.content.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContentMode {
    Edit,
    Preview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Preview {
    Loading,
    Ready(String),
    Failed,
}

/// Why Save did not write.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SaveRefusal {
    NameRequired,
    Name(NameProblem),
    ContentRequired,
}

/// One open editor: a saved snippet (by its file) or a draft.
struct Editor {
    generation: u64,
    /// The file it edits; `None` for a draft not yet created.
    file: Option<String>,
    /// The name the server knows it by, for a rename or a move.
    saved_name: Option<String>,
    initial: Form,
    scope: SnippetScope,
    aliases: Vec<String>,
    name: Entity<InputState>,
    description: Entity<InputState>,
    alias: Entity<InputState>,
    content: Entity<TextareaState>,
    scope_picker: Entity<SelectState<SearchableVec<ScopeOption>>>,
    mode: ContentMode,
    preview: Preview,
    preview_task: Option<Task<()>>,
    saving: bool,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, PartialEq, Eq)]
struct ScopeOption {
    value: &'static str,
    label: SharedString,
}

impl SearchableListItem for ScopeOption {
    type Value = &'static str;
    fn title(&self) -> SharedString {
        self.label.clone()
    }
    fn value(&self) -> &&'static str {
        &self.value
    }
}

fn scope_label(scope: SnippetScope) -> &'static str {
    match scope {
        SnippetScope::Project => "settings.common.scope.project",
        SnippetScope::Global => "settings.common.scope.global",
    }
}

pub(crate) struct SnippetsPage {
    owner: WeakEntity<Workspace>,
    network: Option<Network>,
    target: Option<ServerTarget>,
    directory: Option<String>,
    list: Load<Vec<Snippet>>,
    /// The selected snippet's name, or the draft's.
    selected: Option<String>,
    editor: Option<Editor>,
    editor_generation: u64,
    read_revision: u64,
    read: Option<Task<()>>,
    writes: SettingsWrites,
    save_failed: bool,
    /// Feedback of the last action, kept for tests and the status line.
    notice: Option<SharedString>,
    list_scroll: ScrollHandle,
    scroll: ScrollHandle,
    focus: HashMap<SharedString, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl SnippetsPage {
    pub(crate) fn new(owner: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(workspace) = owner.upgrade() {
            subscriptions.push(cx.observe(&workspace, |this, _, cx| {
                let target = ServerTarget::capture(&this.owner, this.network.as_ref(), cx);
                if !same_target(this.target.as_ref(), target.as_ref())
                    || this.directory != directory(&this.owner, cx)
                {
                    this.refresh(cx);
                }
            }));
        }
        Self {
            owner,
            network: Network::new().ok(),
            target: None,
            directory: None,
            list: Load::Loading,
            selected: None,
            editor: None,
            editor_generation: 0,
            read_revision: 0,
            read: None,
            writes: SettingsWrites::default(),
            save_failed: false,
            notice: None,
            list_scroll: ScrollHandle::new(),
            scroll: ScrollHandle::new(),
            focus: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Reads the list for the current server and directory.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let target = ServerTarget::capture(&self.owner, self.network.as_ref(), cx);
        let directory = directory(&self.owner, cx);
        if !same_target(self.target.as_ref(), target.as_ref()) || self.directory != directory {
            // Another server's or project's snippets never carry over.
            self.list = Load::Loading;
            self.selected = None;
            self.editor = None;
            self.save_failed = false;
        }
        self.target = target.clone();
        self.directory = directory.clone();
        let Some(target) = target else {
            self.read = None;
            cx.notify();
            return;
        };
        if self.list == Load::Failed {
            self.list = Load::Loading;
        }
        self.read_revision += 1;
        let revision = self.read_revision;
        let client = target.client;
        let reading = NetworkTask::spawn_on(&target.network, async move {
            client.snippets(directory.as_deref()).await
        });
        self.read = Some(cx.spawn(async move |this, cx| {
            let result = reading.await;
            let _ = this.update(cx, |this, cx| {
                if this.read_revision != revision {
                    return;
                }
                this.read = None;
                match result {
                    Some(Ok(snippets)) => this.list = Load::Ready(snippets),
                    // A list already shown stays; a first read fails visibly.
                    _ if this.list.ready().is_some() => {}
                    _ => this.list = Load::Failed,
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn snippets(&self) -> &[Snippet] {
        self.list.ready().map_or(&[], Vec::as_slice)
    }

    fn form(&self, cx: &App) -> Option<Form> {
        let editor = self.editor.as_ref()?;
        Some(Form {
            name: editor.name.read(cx).value().to_string(),
            scope: editor.scope,
            description: editor.description.read(cx).value().to_string(),
            aliases: editor.aliases.clone(),
            content: editor.content.read(cx).value().to_string(),
        })
    }

    fn has_unsaved_changes(&self, cx: &App) -> bool {
        self.editor
            .as_ref()
            .zip(self.form(cx))
            .is_some_and(|(editor, form)| form != editor.initial)
    }

    /// Runs `open` now, or after the user agrees to drop unsaved edits.
    fn switch_to(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        open: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if !self.has_unsaved_changes(cx) {
            open(self, window, cx);
            return;
        }
        let strings = locale::strings(cx);
        let name = self.selected.clone().unwrap_or_default();
        confirm(
            cx.weak_entity(),
            strings.get("settings.snippets.sidebar.dialog.discardTitle"),
            strings.format(
                "settings.snippets.sidebar.dialog.discardDescription",
                &[("name", &name)],
            ),
            strings.get("settings.snippets.sidebar.dialog.discard"),
            window,
            cx,
            open,
        );
    }

    fn create_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.list.ready().is_none() {
            return;
        }
        self.switch_to(window, cx, |this, window, cx| {
            let name = unique_snippet_name("new-snippet", this.snippets());
            let form = Form {
                name: name.clone(),
                ..Form::default()
            };
            this.open_editor(None, None, form, window, cx);
            this.selected = Some(name);
        });
    }

    fn select(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(name.as_str()) && self.editor.is_some() {
            return;
        }
        self.switch_to(window, cx, move |this, window, cx| {
            let Some(snippet) = this
                .snippets()
                .iter()
                .find(|snippet| snippet.name == name)
                .cloned()
            else {
                return;
            };
            this.open_editor(
                Some(snippet.file_path.clone()),
                Some(snippet.name.clone()),
                Form::of(&snippet),
                window,
                cx,
            );
            this.selected = Some(name.clone());
        });
    }

    fn open_editor(
        &mut self,
        file: Option<String>,
        saved_name: Option<String>,
        form: Form,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let strings = locale::strings(cx).clone();
        let input = |value: &str,
                     placeholder: SharedString,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            let value = value.to_owned();
            cx.new(|cx| {
                let mut state = InputState::new(window, cx);
                state.set_placeholder(placeholder, window, cx);
                state.set_value(value, window, cx);
                state
            })
        };
        let name = input(
            &form.name,
            strings.get("settings.snippets.page.field.namePlaceholder"),
            window,
            cx,
        );
        let description = input(
            &form.description,
            strings.get("settings.snippets.page.field.descriptionPlaceholder"),
            window,
            cx,
        );
        let alias = input(
            "",
            strings.get("settings.snippets.page.field.aliasPlaceholder"),
            window,
            cx,
        );
        let content = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .auto_grow(10, 40)
                .placeholder(strings.get("settings.snippets.page.field.contentPlaceholder"));
            state.set_value(form.content.clone(), window, cx);
            state
        });
        let options = [SnippetScope::Global, SnippetScope::Project]
            .map(|scope| ScopeOption {
                value: match scope {
                    SnippetScope::Global => "global",
                    SnippetScope::Project => "project",
                },
                label: strings.get(scope_label(scope)),
            })
            .to_vec();
        let selected = usize::from(form.scope == SnippetScope::Project);
        let scope_picker = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(options),
                Some(IndexPath::new(selected)),
                window,
                cx,
            )
        });
        let subscriptions = vec![
            // The name field keeps only what a name may hold.
            cx.subscribe_in(&name, window, |_, input, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    let normalized = normalize_snippet_name_input(&value);
                    if normalized != value {
                        input.update(cx, |input, cx| input.set_value(normalized, window, cx));
                    }
                }
                cx.notify();
            }),
            cx.subscribe_in(&description, window, |_, _, _: &InputEvent, _, cx| {
                cx.notify()
            }),
            cx.subscribe_in(&content, window, |_, _, _: &InputEvent, _, cx| cx.notify()),
            cx.subscribe_in(&alias, window, |this, input, event, window, cx| {
                let value = input.read(cx).value().to_string();
                match event {
                    // A comma or a space ends an alias, typed or pasted.
                    InputEvent::Change if value.contains([',', ' ', '\t', '\n']) => {
                        let mut parts: Vec<String> = value
                            .split([',', ' ', '\t', '\n'])
                            .map(normalize_snippet_name_input)
                            .collect();
                        let rest = parts.pop().unwrap_or_default();
                        this.add_aliases(parts, cx);
                        input.update(cx, |input, cx| input.set_value(rest, window, cx));
                    }
                    InputEvent::PressEnter { .. } | InputEvent::Blur if !value.is_empty() => {
                        this.add_aliases(vec![normalize_snippet_name_input(&value)], cx);
                        input.update(cx, |input, cx| input.set_value("", window, cx));
                    }
                    _ => {}
                }
            }),
            cx.subscribe_in(&scope_picker, window, |this, _, event, _, cx| {
                if let SelectEvent::Confirm(Some(value)) = event
                    && let Some(editor) = &mut this.editor
                {
                    editor.scope = if *value == "project" {
                        SnippetScope::Project
                    } else {
                        SnippetScope::Global
                    };
                    cx.notify();
                }
            }),
        ];
        self.editor_generation += 1;
        self.save_failed = false;
        self.editor = Some(Editor {
            generation: self.editor_generation,
            file,
            saved_name,
            scope: form.scope,
            aliases: form.aliases.clone(),
            initial: form,
            name,
            description,
            alias,
            content,
            scope_picker,
            mode: ContentMode::Edit,
            preview: Preview::Loading,
            preview_task: None,
            saving: false,
            _subscriptions: subscriptions,
        });
        self.scroll.set_offset(gpui_kit::point(px(0.0), px(0.0)));
        cx.notify();
    }

    /// Adds the valid, new ones among `candidates`.
    fn add_aliases(&mut self, candidates: Vec<String>, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        for candidate in candidates {
            if is_valid_snippet_name(&candidate)
                && !editor
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(&candidate))
            {
                editor.aliases.push(candidate);
            }
        }
        cx.notify();
    }

    fn remove_alias(&mut self, alias: &str, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.editor {
            editor.aliases.retain(|existing| existing != alias);
            cx.notify();
        }
    }

    fn set_mode(&mut self, mode: ContentMode, cx: &mut Context<Self>) {
        let Some(content) = self.form(cx).map(|form| form.content) else {
            return;
        };
        let target = self.target.clone();
        let directory = self.directory.clone();
        let Some(editor) = &mut self.editor else {
            return;
        };
        editor.mode = mode;
        editor.preview_task = None;
        if mode == ContentMode::Preview {
            editor.preview = Preview::Loading;
            let Some(target) = target else {
                editor.preview = Preview::Failed;
                cx.notify();
                return;
            };
            let client = target.client;
            let reading = NetworkTask::spawn_on(&target.network, async move {
                client.preview_snippet(&content, directory.as_deref()).await
            });
            editor.preview_task = Some(cx.spawn(async move |this, cx| {
                let result = reading.await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(editor) = &mut this.editor {
                        editor.preview = match result {
                            Some(Ok(text)) => Preview::Ready(text),
                            _ => Preview::Failed,
                        };
                        cx.notify();
                    }
                });
            }));
        }
        cx.notify();
    }

    /// The checks Save runs before it writes (`handleSave`).
    fn check(&self, form: &Form) -> Result<(), SaveRefusal> {
        let name = form.name.trim();
        if name.is_empty() {
            return Err(SaveRefusal::NameRequired);
        }
        let own = self
            .editor
            .as_ref()
            .and_then(|editor| editor.file.as_deref());
        if let Some(problem) = snippet_name_problem(name, self.snippets(), own) {
            return Err(SaveRefusal::Name(problem));
        }
        if form.content.trim().is_empty() {
            return Err(SaveRefusal::ContentRequired);
        }
        Ok(())
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() {
            return;
        }
        let (Some(form), Some(target)) = (self.form(cx), self.target.clone()) else {
            return;
        };
        let Some(editor) = &self.editor else {
            return;
        };
        if editor.saving || (editor.file.is_some() && form == editor.initial) {
            return;
        }
        let name = form.name.trim().to_owned();
        if let Err(refusal) = self.check(&form) {
            let strings = locale::strings(cx);
            let text = match refusal {
                SaveRefusal::NameRequired => {
                    strings.get("settings.snippets.page.toast.nameRequired")
                }
                SaveRefusal::Name(NameProblem::Taken) => {
                    strings.format("snippets.name.taken", &[("name", &name)])
                }
                SaveRefusal::Name(NameProblem::Invalid) => strings.get("snippets.name.invalid"),
                SaveRefusal::ContentRequired => {
                    strings.get("settings.snippets.page.toast.contentRequired")
                }
            };
            self.toast(text, false, window, cx);
            return;
        }
        let editor = self.editor.as_mut().expect("checked above");
        editor.saving = true;
        let generation = editor.generation;
        let file = editor.file.clone();
        let saved_name = editor.saved_name.clone();
        let original_scope = editor.initial.scope;
        let fields = SnippetFields {
            content: form.content.clone(),
            aliases: form.aliases.clone(),
            description: form.description.clone(),
        };
        let directory = self.directory.clone();
        let client = target.client.clone();
        let write_directory = directory.clone();
        let write_name = name.clone();
        let (id, writing) = self.writes.submit(&target, async move {
            match saved_name {
                Some(saved) => client
                    .update_snippet(
                        &saved,
                        &fields,
                        (write_name != saved).then_some(write_name.as_str()),
                        (form.scope != original_scope).then_some(form.scope),
                        write_directory.as_deref(),
                    )
                    .await
                    .map(|_| ()),
                None => {
                    client
                        .create_snippet(
                            &write_name,
                            &fields,
                            form.scope,
                            write_directory.as_deref(),
                        )
                        .await
                }
            }
        });
        let submitted = self.form(cx).expect("an editor is open");
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if !same_target(this.target.as_ref(), Some(&target))
                    || !same_target(
                        ServerTarget::capture(&this.owner, this.network.as_ref(), cx).as_ref(),
                        Some(&target),
                    )
                    || this.directory != directory
                {
                    return;
                }
                if !this
                    .editor
                    .as_ref()
                    .is_some_and(|editor| editor.generation == generation)
                {
                    if matches!(result, Some(Ok(()))) {
                        this.reload_after_write(None, cx);
                    }
                    return;
                }
                let strings = locale::strings(cx).clone();
                if let Some(editor) = &mut this.editor {
                    editor.saving = false;
                }
                this.save_failed = !matches!(result, Some(Ok(())));
                match result {
                    Some(Ok(())) => {
                        if let Some(editor) = &mut this.editor {
                            // The saved fields are the new baseline; the
                            // editor now names the snippet's new file.
                            editor.initial = submitted.clone();
                            editor.saved_name = Some(name.clone());
                            editor.file = Some(file.clone().unwrap_or_default());
                            this.selected = Some(name.clone());
                        }
                        this.toast(
                            strings.get("settings.snippets.page.toast.saved"),
                            true,
                            window,
                            cx,
                        );
                        this.reload_after_write(Some(name.clone()), cx);
                    }
                    Some(Err(SnippetError::NameTaken)) => this.toast(
                        strings.format("snippets.name.taken", &[("name", &name)]),
                        false,
                        window,
                        cx,
                    ),
                    _ => this.toast(
                        strings.get("settings.snippets.page.toast.saveFailed"),
                        false,
                        window,
                        cx,
                    ),
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    /// Re-reads the list; the open editor follows `name` to its file.
    fn reload_after_write(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        let (Some(target), directory) = (self.target.clone(), self.directory.clone()) else {
            return;
        };
        self.read_revision += 1;
        let revision = self.read_revision;
        let client = target.client;
        let reading = NetworkTask::spawn_on(&target.network, async move {
            client.snippets(directory.as_deref()).await
        });
        self.read = Some(cx.spawn(async move |this, cx| {
            let result = reading.await;
            let _ = this.update(cx, |this, cx| {
                if this.read_revision != revision {
                    return;
                }
                this.read = None;
                if let Some(Ok(snippets)) = result {
                    if let (Some(name), Some(editor)) = (&name, &mut this.editor)
                        && let Some(saved) = snippets.iter().find(|snippet| &snippet.name == name)
                        && editor.saved_name.as_deref() == Some(name.as_str())
                    {
                        editor.file = Some(saved.file_path.clone());
                    }
                    this.list = Load::Ready(snippets);
                }
                cx.notify();
            });
        }));
    }

    fn ask_delete(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let strings = locale::strings(cx);
        confirm(
            cx.weak_entity(),
            strings.get("settings.snippets.sidebar.dialog.deleteTitle"),
            strings.format(
                "settings.snippets.sidebar.dialog.deleteDescription",
                &[("name", &name)],
            ),
            strings.get("settings.common.actions.delete"),
            window,
            cx,
            move |this, window, cx| this.delete(name.clone(), window, cx),
        );
    }

    fn delete(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        let directory = self.directory.clone();
        let client = target.client.clone();
        let write_directory = directory.clone();
        let deleted = name.clone();
        let (id, writing) = self.writes.submit(&target, async move {
            client
                .delete_snippet(&deleted, write_directory.as_deref())
                .await
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if !same_target(this.target.as_ref(), Some(&target))
                    || !same_target(
                        ServerTarget::capture(&this.owner, this.network.as_ref(), cx).as_ref(),
                        Some(&target),
                    )
                    || this.directory != directory
                {
                    return;
                }
                let strings = locale::strings(cx).clone();
                if matches!(result, Some(Ok(()))) {
                    if this.selected.as_deref() == Some(name.as_str()) {
                        this.selected = None;
                        this.editor = None;
                        this.save_failed = false;
                    }
                    this.toast(
                        strings.get("settings.snippets.sidebar.toast.deleted"),
                        true,
                        window,
                        cx,
                    );
                    this.reload_after_write(None, cx);
                } else {
                    this.toast(
                        strings.get("settings.snippets.sidebar.toast.deleteFailed"),
                        false,
                        window,
                        cx,
                    );
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
    }

    pub(crate) fn take_pending_write(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let pending = self.writes.take_pending_write();
        cx.notify();
        pending
    }

    pub(crate) fn has_failed_writes(&self) -> bool {
        self.save_failed
    }

    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.writes.cancel_close();
        cx.notify();
    }

    pub(crate) fn take_quit_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.writes.take_quit_write(self.network.take(), cx)
    }

    fn toast(
        &mut self,
        text: SharedString,
        success: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notice = Some(text.clone());
        let notification = if success {
            Notification::success(text)
        } else {
            Notification::error(text)
        };
        window.push_notification(notification, cx);
    }
}

impl SnippetsPage {
    /// `snippets.create` starts a draft; `snippets.content` focuses the
    /// open editor's content.
    pub(crate) fn reveal(&mut self, item: &str, window: &mut Window, cx: &mut Context<Self>) {
        match item {
            "snippets.create" => self.create_new(window, cx),
            "snippets.content" => {
                if let Some(editor) = &self.editor {
                    let content = editor.content.clone();
                    content.update(cx, |input, cx| input.focus(window, cx));
                }
            }
            _ => {}
        }
    }
}

/// The directory a snippet list is read for: the workspace's config
/// directory, as `getRequestDirectory` resolves the current one.
fn directory(owner: &WeakEntity<Workspace>, cx: &App) -> Option<String> {
    owner.upgrade()?.read(cx).config_directory()
}

impl SnippetsPage {
    fn icon_button(
        &mut self,
        id: SharedString,
        icon: &'static str,
        label: SharedString,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let palette = cx.global::<Palette>().clone();
        let focus = focus_handle(&mut self.focus, &id, cx);
        on_activate(
            div()
                .id(id.clone())
                .accessibility_id(id)
                .role(Role::Button)
                .aria_label(label)
                .track_focus(&focus)
                .tab_index(0)
                .size(px(24.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.0))
                .cursor_pointer()
                .hover(move |style| style.bg(palette.hover))
                .child(glyph(icon, 14.0, palette.muted_foreground)),
            cx,
            action,
        )
        .test_support()
        .into_any_element()
    }

    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let count = self.snippets().len().to_string();
        let create = self.icon_button(
            "snippets-create".into(),
            "add",
            strings.get("settings.snippets.sidebar.actions.create"),
            cx,
            |this, window, cx| this.create_new(window, cx),
        );
        let mut sorted: Vec<Snippet> = self.snippets().to_vec();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        let mut list = div()
            .id("snippets-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(12.0));
        match &self.list {
            Load::Failed => {
                list = list.child(load_failed(
                    "snippets-load-failed",
                    strings.get("settings.openchamber.desktopNetwork.error.loadFailed"),
                    strings.get("desktopHostSwitcher.actions.retry"),
                    cx,
                    |this, cx| this.refresh(cx),
                ));
            }
            Load::Loading => {}
            Load::Ready(_) => {
                for scope in [SnippetScope::Project, SnippetScope::Global] {
                    let group: Vec<&Snippet> = sorted
                        .iter()
                        .filter(|snippet| snippet.source == scope)
                        .collect();
                    if group.is_empty() {
                        continue;
                    }
                    let mut column = div().flex().flex_col().gap(px(2.0)).child(
                        div()
                            .px(px(6.0))
                            .pt(px(4.0))
                            .css_text(0.6875, 1.0, cx)
                            .text_color(palette.muted_foreground)
                            .child(strings.get(scope_label(scope))),
                    );
                    for snippet in group {
                        column = column.child(self.row(snippet, cx));
                    }
                    list = list.child(column);
                }
            }
        }
        div()
            .id("snippets-sidebar")
            .w(px(256.0))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .border_color(palette.border)
            .when(direction == Direction::LeftToRight, |side| {
                side.border_r_1()
            })
            .when(direction == Direction::RightToLeft, |side| {
                side.border_l_1()
            })
            .child(
                div()
                    .px(px(12.0))
                    .pt(px(16.0))
                    .pb(px(12.0))
                    .border_b_1()
                    .border_color(palette.border)
                    .child(
                        div()
                            .id("snippets-sidebar-title")
                            .role(Role::Heading)
                            .font_weight(FontWeight::SEMIBOLD)
                            .mb(px(12.0))
                            .child(strings.get("settings.snippets.sidebar.title")),
                    )
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .justify_between()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .css_text(0.8125, 1.25, cx)
                                    .text_color(palette.muted_foreground)
                                    .child(strings.format(
                                        "settings.snippets.sidebar.total",
                                        &[("count", &count)],
                                    )),
                            )
                            .child(create),
                    ),
            )
            .child(list)
            .into_any_element()
    }

    fn row(&mut self, snippet: &Snippet, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let selected = self.selected.as_deref() == Some(snippet.name.as_str());
        let id: SharedString = format!("snippets-row-{}", snippet.name).into();
        let focus = focus_handle(&mut self.focus, &id, cx);
        let summary = snippet
            .description
            .clone()
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| {
                snippet
                    .content
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(80)
                    .collect()
            });
        let name = snippet.name.clone();
        let delete_name = snippet.name.clone();
        let delete = self.icon_button(
            format!("snippets-delete-{}", snippet.name).into(),
            "delete-bin",
            strings.format(
                "settings.snippets.sidebar.actions.more",
                &[("name", &snippet.name)],
            ),
            cx,
            move |this, window, cx| this.ask_delete(delete_name.clone(), window, cx),
        );
        div()
            .flex_row_from(direction)
            .items_center()
            .rounded(px(6.0))
            .px(px(6.0))
            .py(px(4.0))
            .when(selected, |row| row.bg(palette.selection))
            .child(
                on_activate(
                    div()
                        .id(id.clone())
                        .accessibility_id(id)
                        .role(Role::Button)
                        .aria_label(SharedString::from(format!("#{}", snippet.name)))
                        .aria_selected(selected)
                        .track_focus(&focus)
                        .tab_index(0)
                        .flex_1()
                        .min_w_0()
                        .cursor_pointer()
                        .child(div().truncate().child(format!("#{}", snippet.name)))
                        .child(
                            div()
                                .truncate()
                                .css_text(0.6875, 1.0, cx)
                                .text_color(palette.muted_foreground)
                                .child(summary),
                        ),
                    cx,
                    move |this, window, cx| this.select(name.clone(), window, cx),
                )
                .test_support(),
            )
            .child(delete)
            .into_any_element()
    }

    fn chip(
        &mut self,
        mode: ContentMode,
        current: ContentMode,
        label: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = cx.global::<Palette>().clone();
        let id: SharedString = match mode {
            ContentMode::Edit => "snippets-content-edit".into(),
            ContentMode::Preview => "snippets-content-preview".into(),
        };
        let focus = focus_handle(&mut self.focus, &id, cx);
        let selected = mode == current;
        on_activate(
            div()
                .id(id.clone())
                .accessibility_id(id)
                .role(Role::Button)
                .aria_label(label.clone())
                .aria_toggled(if selected {
                    Toggled::True
                } else {
                    Toggled::False
                })
                .track_focus(&focus)
                .tab_index(0)
                .h_full()
                .px(px(10.0))
                .rounded(px(5.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .when(selected, |chip| {
                    chip.bg(palette.background).font_weight(FontWeight::MEDIUM)
                })
                .child(label),
            cx,
            move |this, _, cx| this.set_mode(mode, cx),
        )
        .test_support()
        .into_any_element()
    }

    fn editor_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let Some(form) = self.form(cx) else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(4.0))
                .text_color(palette.muted_foreground)
                .child(glyph("file-text", 48.0, palette.muted_foreground))
                .child(strings.get("settings.snippets.page.empty.title"))
                .child(
                    div()
                        .css_text(0.8125, 1.25, cx)
                        .child(strings.get("settings.snippets.page.empty.description")),
                )
                .into_any_element();
        };
        let editor = self.editor.as_ref().expect("a form has an editor");
        let is_new = editor.file.is_none();
        let changed = form != editor.initial;
        let can_save = !self.writes.closing() && !editor.saving && (is_new || changed);
        let saving = editor.saving;
        let mode = editor.mode;
        let preview = editor.preview.clone();
        let aliases = editor.aliases.clone();
        let (name, description, alias, content, scope_picker) = (
            editor.name.clone(),
            editor.description.clone(),
            editor.alias.clone(),
            editor.content.clone(),
            editor.scope_picker.clone(),
        );
        let title: SharedString = match &editor.saved_name {
            Some(saved) if !is_new => format!("#{saved}").into(),
            _ => strings.get("settings.snippets.page.title.new"),
        };
        let path = editor.file.clone().filter(|file| !file.is_empty());
        let name_problem = (!form.name.is_empty())
            .then(|| snippet_name_problem(&form.name, self.snippets(), editor.file.as_deref()))
            .flatten();
        let stacked = |label: SharedString, control: AnyElement| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(div().font_weight(FontWeight::MEDIUM).child(label))
                .child(control)
                .into_any_element()
        };
        let name_field = div()
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .w_full()
            .max_w(px(384.0))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&name)
                        .id("snippets-name")
                        .accessibility_id("snippets-name")
                        .aria_label(strings.get("snippets.field.name")),
                ),
            )
            .child(
                div().w(px(120.0)).key_context("SettingsPicker").child(
                    Select::new(&scope_picker)
                        .id("snippets-scope")
                        .accessibility_label(strings.get("snippets.field.scopeAria"))
                        .w_full(),
                ),
            )
            .into_any_element();
        let mut alias_chips = div()
            .w_full()
            .max_w(px(384.0))
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(4.0));
        for value in &aliases {
            let removed = value.clone();
            let remove = self.icon_button(
                format!("snippets-alias-remove-{value}").into(),
                "close",
                strings.format(
                    "settings.snippets.page.field.aliasRemove",
                    &[("alias", value)],
                ),
                cx,
                move |this, _, cx| this.remove_alias(&removed, cx),
            );
            alias_chips = alias_chips.child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(2.0))
                    .padding_start(direction, px(6.0))
                    .rounded(px(4.0))
                    .bg(palette.elevated)
                    .css_text(0.8125, 1.25, cx)
                    .child(format!("#{value}"))
                    .child(remove),
            );
        }
        alias_chips = alias_chips.child(
            div().min_w(px(96.0)).flex_1().child(
                Input::new(&alias)
                    .id("snippets-alias")
                    .accessibility_id("snippets-alias")
                    .aria_label(strings.get("settings.snippets.page.field.aliases")),
            ),
        );
        let edit_chip = self.chip(
            ContentMode::Edit,
            mode,
            strings.get("settings.snippets.page.content.edit"),
            cx,
        );
        let preview_chip = self.chip(
            ContentMode::Preview,
            mode,
            strings.get("settings.snippets.page.content.preview"),
            cx,
        );
        let mono = Theme::global(cx).mono_font_family.clone();
        let body = match mode {
            ContentMode::Edit => div()
                .css_text(0.8125, 1.25, cx)
                .id("snippets-content")
                .font_family(mono)
                .child(
                    Textarea::new(&content)
                        .accessibility_id("snippets-content-input")
                        .aria_label(strings.get("settings.snippets.page.field.content")),
                )
                .into_any_element(),
            ContentMode::Preview => div()
                .css_text(0.8125, 1.25, cx)
                .id("snippets-preview")
                .min_h(px(240.0))
                .px(px(12.0))
                .py(px(10.0))
                .rounded(px(10.0))
                .bg(palette.elevated)
                .child(match preview {
                    Preview::Ready(text) if !text.is_empty() => {
                        div().font_family(mono).child(text).into_any_element()
                    }
                    Preview::Ready(_) => {
                        helper(strings.get("settings.snippets.page.preview.empty"), cx)
                    }
                    Preview::Failed => {
                        helper(strings.get("settings.snippets.page.preview.failed"), cx)
                    }
                    Preview::Loading => div().into_any_element(),
                })
                .into_any_element(),
        };
        let save_label = if saving {
            strings.get("settings.common.actions.saving")
        } else {
            strings.get("settings.common.actions.saveChanges")
        };
        let save = on_activate(
            gpui_kit::base::Button::new("snippets-save")
                .accessibility_id("snippets-save")
                .accessibility_label(save_label.clone())
                .disabled(!can_save)
                .h(gpui_kit::rems(1.75))
                .px(px(8.0))
                .rounded(px(7.0))
                .child(save_label),
            cx,
            |this, window, cx| this.save(window, cx),
        );
        div()
            .css_text(0.875, 1.25, cx)
            .id("snippets-editor")
            .size_full()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(48.0))
            .py(px(32.0))
            .when(direction == Direction::RightToLeft, |view| {
                view.text_right()
            })
            .child(
                div()
                    .mb(px(24.0))
                    .child(
                        div()
                            .id("snippets-title")
                            .role(Role::Heading)
                            .css_text(1.125, 1.5, cx)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .children(path.map(|path| {
                        div()
                            .mt(px(4.0))
                            .text_color(palette.muted_foreground)
                            .child(path)
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(16.0))
                    .child(stacked(strings.get("snippets.field.name"), name_field))
                    .children(name_problem.map(|problem| {
                        div()
                            .css_text(0.8125, 1.25, cx)
                            .text_color(palette.status_error)
                            .child(match problem {
                                NameProblem::Taken => {
                                    strings.format("snippets.name.taken", &[("name", &form.name)])
                                }
                                NameProblem::Invalid => strings.get("snippets.name.invalid"),
                            })
                    }))
                    .child(stacked(
                        strings.get("settings.common.field.description"),
                        div()
                            .w_full()
                            .max_w(px(384.0))
                            .child(
                                Input::new(&description)
                                    .id("snippets-description")
                                    .aria_label(strings.get("settings.common.field.description")),
                            )
                            .into_any_element(),
                    ))
                    .child(stacked(
                        strings.get("settings.snippets.page.field.aliases"),
                        alias_chips.into_any_element(),
                    )),
            )
            .child(
                div()
                    .mt(px(32.0))
                    .pt(px(24.0))
                    .border_t_1()
                    .border_color(palette.border)
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .justify_between()
                            .gap(px(8.0))
                            .mb(px(12.0))
                            .child(
                                div()
                                    .flex_row_from(direction)
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .id("snippets-content-title")
                                            .role(Role::Heading)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(
                                                strings.get("settings.snippets.page.field.content"),
                                            ),
                                    )
                                    .child(crate::settings_controls::info_hint(
                                        "snippets.content",
                                        strings.get("settings.snippets.page.contentHint"),
                                        cx,
                                    )),
                            )
                            .child(
                                div()
                                    .id("snippets-content-mode")
                                    .role(Role::Group)
                                    .aria_label(
                                        strings.get("settings.snippets.page.content.modeAria"),
                                    )
                                    .flex_row_from(direction)
                                    .h(gpui_kit::rems(2.0))
                                    .p(px(2.0))
                                    .gap(px(2.0))
                                    .rounded(px(7.0))
                                    .bg(palette.elevated)
                                    .child(edit_chip)
                                    .child(preview_chip),
                            ),
                    )
                    .child(body)
                    .child(div().pt(px(12.0)).child(save)),
            )
            .into_any_element()
    }
}

impl Render for SnippetsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        if self.target.is_none() {
            return div()
                .id("snippets-settings-page")
                .size_full()
                .px(px(48.0))
                .py(px(32.0))
                .child(crate::settings_view::page_header(Page::Snippets, cx))
                .child(helper(strings.get("common.unavailable"), cx))
                .into_any_element();
        }
        let sidebar = self.sidebar(cx);
        let editor = self.editor_view(cx);
        div()
            .id("snippets-settings-page")
            .role(Role::Group)
            .aria_label(strings.get(Page::Snippets.title()))
            .size_full()
            .flex_row_from(strings.direction())
            .text_color(palette.foreground)
            .child(sidebar)
            .child(div().flex_1().min_w_0().h_full().child(editor))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, TestAppContext};
    use serde_json::{Value, json};

    use super::*;
    use crate::settings_test_server::{Server, open, settings, settle};

    fn entry(name: &str, source: &str, content: &str) -> Value {
        json!({
            "name": name,
            "content": content,
            "aliases": [],
            "filePath": format!("/{source}/{name}.md"),
            "source": source,
        })
    }

    /// A snippet store: list, create (409 for an existing name), update,
    /// delete and preview.
    fn server(fail_list: bool) -> (Server, Arc<Mutex<Vec<Value>>>) {
        let store = Arc::new(Mutex::new(vec![
            entry("review", "global", "Review this"),
            entry("plan", "project", "Plan it"),
        ]));
        let held = Arc::clone(&store);
        let server = Server::new(move |request| {
            let mut store = held.lock().expect("store");
            let path = request
                .target
                .split('?')
                .next()
                .unwrap_or_default()
                .to_owned();
            let name = path.trim_start_matches("/api/config/snippets/").to_owned();
            match request.method.as_str() {
                "GET" if fail_list => (500, json!({"error": "fixture read failure"})),
                "GET" => (200, Value::Array(store.clone())),
                "POST" if name == "expand" => (
                    200,
                    json!({"text": format!("<{}>", request.body["content"].as_str().unwrap_or_default())}),
                ),
                "POST" if store.iter().any(|snippet| snippet["name"] == name.as_str()) => {
                    (409, json!({"error": "already exists"}))
                }
                "POST" => {
                    let scope = request.body["scope"]
                        .as_str()
                        .unwrap_or("global")
                        .to_owned();
                    let content = request.body["content"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    store.push(entry(&name, &scope, &content));
                    (200, json!({"success": true}))
                }
                "PATCH" => {
                    let Some(snippet) = store
                        .iter_mut()
                        .find(|snippet| snippet["name"] == name.as_str())
                    else {
                        return (404, json!({"error": "not found"}));
                    };
                    snippet["content"] = request.body["content"].clone();
                    if let Some(rename) = request.body["name"].as_str() {
                        snippet["name"] = json!(rename);
                    }
                    let saved = snippet.clone();
                    (200, json!({"success": true, "snippet": saved}))
                }
                "DELETE" => {
                    store.retain(|snippet| snippet["name"] != name.as_str());
                    (200, json!({"success": true}))
                }
                _ => (404, json!({"error": "not in this fixture"})),
            }
        });
        (server, store)
    }

    fn page(workspace: &Entity<Workspace>, cx: &TestAppContext) -> Entity<SnippetsPage> {
        let settings = settings(workspace, cx);
        cx.read(|cx| settings.read(cx).snippets.clone())
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ClosePath {
        Window,
        StandaloneQuit,
        WorkspaceQuit,
    }

    fn accepted_action(delete: bool, path: ClosePath, cx: &mut TestAppContext) {
        use crate::settings_controls::tests::ResponseGate;
        let gate = ResponseGate::new();
        let held = Arc::clone(&gate);
        let server = Server::new(move |request| {
            if request.method == "GET" {
                return (200, json!([entry("review", "global", "Review")]));
            }
            held.hold();
            (200, json!({"success": true}))
        });
        let (window, workspace) = open(&server, Page::Snippets, cx);
        let page = page(&workspace, cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                if delete {
                    page.delete("review".into(), window, cx);
                } else {
                    page.open_editor(
                        None,
                        None,
                        Form {
                            name: "accepted".into(),
                            content: "Submitted content".into(),
                            ..Form::default()
                        },
                        window,
                        cx,
                    );
                    page.save(window, cx);
                }
            });
        })
        .expect("test window");
        gate.entered(cx);
        let settings = settings(&workspace, cx);
        if path != ClosePath::Window {
            if path == ClosePath::StandaloneQuit {
                // Keep this standalone receipt out of the production workspace
                // observer, which otherwise transfers the same I/O first.
                workspace.update(cx, |workspace, _| workspace.settings = None);
            } else {
                assert!(
                    settings
                        .update(cx, |settings, cx| settings.take_pending_write(cx))
                        .is_some(),
                    "normal close exposes the accepted action before production quit takes custody"
                );
            }
            cx.update(|cx| {
                cx.on_app_quit(move |cx| {
                    let pending = if path == ClosePath::StandaloneQuit {
                        settings.update(cx, |settings, cx| settings.take_quit_write(cx))
                    } else {
                        None
                    };
                    gate.release();
                    if path == ClosePath::StandaloneQuit {
                        assert!(
                            pending.is_some(),
                            "quit retains the accepted explicit snippet action"
                        );
                    }
                    async move {
                        if let Some(pending) = pending {
                            pending.await;
                        }
                    }
                })
                .detach();
            });
            cx.executor().allow_parking();
            cx.quit();
        } else {
            let pending = settings.update(cx, |settings, cx| settings.take_pending_write(cx));
            gate.release();
            assert!(
                pending.is_some(),
                "normal close waits for the accepted explicit snippet action"
            );
            cx.foreground_executor().block_test(async move {
                pending.expect("close receipt").await;
            });
        }
        assert_eq!(
            server.writes().len(),
            1,
            "the accepted action completes exactly once"
        );
    }

    #[gpui_kit::test]
    fn accepted_lifecycle_snippet_save_waits_on_normal_close(cx: &mut TestAppContext) {
        accepted_action(false, ClosePath::Window, cx);
    }

    #[gpui_kit::test]
    fn accepted_lifecycle_snippet_delete_waits_on_normal_close(cx: &mut TestAppContext) {
        accepted_action(true, ClosePath::Window, cx);
    }

    #[gpui_kit::test]
    fn accepted_lifecycle_snippet_save_drains_on_app_quit(cx: &mut TestAppContext) {
        accepted_action(false, ClosePath::StandaloneQuit, cx);
    }

    #[gpui_kit::test]
    fn accepted_lifecycle_snippet_delete_drains_on_app_quit(cx: &mut TestAppContext) {
        accepted_action(true, ClosePath::StandaloneQuit, cx);
    }

    #[gpui_kit::test]
    fn workspace_quit_drains_an_accepted_snippet_save(cx: &mut TestAppContext) {
        accepted_action(false, ClosePath::WorkspaceQuit, cx);
    }

    #[gpui_kit::test]
    fn workspace_quit_drains_an_accepted_snippet_delete(cx: &mut TestAppContext) {
        accepted_action(true, ClosePath::WorkspaceQuit, cx);
    }

    fn act(
        window: AnyWindowHandle,
        page: &Entity<SnippetsPage>,
        cx: &mut TestAppContext,
        action: impl FnOnce(&mut SnippetsPage, &mut Window, &mut Context<SnippetsPage>),
    ) {
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| action(page, window, cx));
        })
        .expect("test window");
        settle(window, cx);
    }

    fn set_content(
        page: &mut SnippetsPage,
        text: &str,
        window: &mut Window,
        cx: &mut Context<SnippetsPage>,
    ) {
        let content = page.editor.as_ref().expect("editor").content.clone();
        let text = text.to_owned();
        content.update(cx, |input, cx| input.set_value(text, window, cx));
    }

    #[gpui_kit::test]
    fn review_regression_a_new_save_cannot_attach_to_a_later_new_editor(cx: &mut TestAppContext) {
        let (server, _) = server(false);
        let (window, workspace) = open(&server, Page::Snippets, cx);
        let page = page(&workspace, cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.open_editor(
                    None,
                    None,
                    Form {
                        name: "first".into(),
                        content: "First body".into(),
                        ..Form::default()
                    },
                    window,
                    cx,
                );
                page.save(window, cx);
                // The discard confirmation opens a fresh editor before completion.
                page.open_editor(
                    None,
                    None,
                    Form {
                        name: "second".into(),
                        content: "Second body".into(),
                        ..Form::default()
                    },
                    window,
                    cx,
                );
                page.selected = Some("second".into());
            });
        })
        .expect("test window");
        settle(window, cx);
        cx.read(|cx| {
            let page = page.read(cx);
            let editor = page.editor.as_ref().expect("second editor");
            assert_eq!(
                editor.saved_name, None,
                "the second draft still creates its own file"
            );
            assert_eq!(page.selected.as_deref(), Some("second"));
        });
        act(window, &page, cx, |page, window, cx| page.save(window, cx));
        assert!(
            server
                .writes()
                .iter()
                .any(|write| write.is("POST", "/api/config/snippets/second"))
        );
        assert!(
            !server
                .writes()
                .iter()
                .any(|write| write.is("PATCH", "/api/config/snippets/first"))
        );
    }

    // The replacement happens in one foreground update, before Save's callback
    // can run, just as the confirmed discard callback opens another editor.
    fn assert_list_reconciles_after_editor_switch(rename: bool, cx: &mut TestAppContext) {
        let (server, store) = server(false);
        let (window, workspace) = open(&server, Page::Snippets, cx);
        let page = page(&workspace, cx);
        let saved_name = if rename { "renamed-review" } else { "created" };
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                if rename {
                    page.select("review".into(), window, cx);
                } else {
                    page.create_new(window, cx);
                }
                let name = page.editor.as_ref().expect("submitted editor").name.clone();
                name.update(cx, |input, cx| input.set_value(saved_name, window, cx));
                set_content(page, "Submitted body", window, cx);
                page.save(window, cx);

                let next = page
                    .snippets()
                    .iter()
                    .find(|snippet| snippet.name == "plan")
                    .cloned()
                    .expect("next saved snippet");
                page.open_editor(
                    Some(next.file_path.clone()),
                    Some(next.name.clone()),
                    Form::of(&next),
                    window,
                    cx,
                );
                page.selected = Some(next.name);
                set_content(page, "Unsaved plan edit", window, cx);
            });
        })
        .expect("test window");
        settle(window, cx);
        assert!(
            store
                .lock()
                .expect("server store")
                .iter()
                .any(|snippet| snippet["name"] == saved_name),
            "the captured write succeeded on the server before list reconciliation is checked"
        );
        cx.read(|cx| {
            let page = page.read(cx);
            assert!(
                page.snippets()
                    .iter()
                    .any(|snippet| snippet.name == saved_name),
                "a successful save reconciles its matching-target list after the editor changes"
            );
            if rename {
                assert!(
                    !page
                        .snippets()
                        .iter()
                        .any(|snippet| snippet.name == "review"),
                    "the list no longer shows the old name after a successful rename"
                );
            }
            let editor = page.editor.as_ref().expect("retained second editor");
            assert_eq!(page.selected.as_deref(), Some("plan"));
            assert_eq!(editor.saved_name.as_deref(), Some("plan"));
            assert_eq!(editor.initial.content, "Plan it");
            assert_eq!(
                page.form(cx).expect("retained form").content,
                "Unsaved plan edit"
            );
            assert!(
                page.has_unsaved_changes(cx),
                "list reconciliation preserves the active unsaved editor"
            );
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let row = if rename {
                "snippets-row-renamed-review"
            } else {
                "snippets-row-created"
            };
            assert!(
                window.try_find(row).is_some(),
                "the newly saved name appears in the visible list without a manual refresh"
            );
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn completed_create_reconciles_the_list_after_switching_editors(cx: &mut TestAppContext) {
        assert_list_reconciles_after_editor_switch(false, cx);
    }

    #[gpui_kit::test]
    fn completed_rename_reconciles_the_list_after_switching_editors(cx: &mut TestAppContext) {
        assert_list_reconciles_after_editor_switch(true, cx);
    }

    #[gpui_kit::test]
    fn an_existing_editor_keeps_text_typed_after_save_started(cx: &mut TestAppContext) {
        let (server, _) = server(false);
        let (window, workspace) = open(&server, Page::Snippets, cx);
        let page = page(&workspace, cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.select("review".into(), window, cx);
                set_content(page, "Submitted", window, cx);
                page.save(window, cx);
                set_content(page, "Newer typing", window, cx);
            });
        })
        .expect("test window");
        settle(window, cx);
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.form(cx).expect("form").content, "Newer typing");
            assert!(page.has_unsaved_changes(cx));
        });
        act(window, &page, cx, |page, window, cx| page.save(window, cx));
        assert_eq!(
            server.writes().last().expect("second save").body["content"],
            "Newer typing"
        );
    }

    #[gpui_kit::test]
    fn changing_project_or_runtime_retires_a_pending_snippet_editor(cx: &mut TestAppContext) {
        use crate::settings_controls::tests::retarget;
        for runtime in [false, true] {
            let (first, _) = server(false);
            let (second, _) = server(false);
            let (window, workspace) = open(&first, Page::Snippets, cx);
            let page = page(&workspace, cx);
            cx.update_window(window, |_, window, cx| {
                page.update(cx, |page, cx| {
                    page.open_editor(
                        None,
                        None,
                        Form {
                            name: "old-owner".into(),
                            content: "Old content".into(),
                            ..Form::default()
                        },
                        window,
                        cx,
                    );
                    page.save(window, cx);
                });
                if !runtime {
                    workspace.update(cx, |workspace, cx| {
                        workspace.show_new_session(window, cx);
                        let directory = workspace
                            .topology
                            .projects
                            .iter()
                            .map(|project| project.path.clone())
                            .find(|path| Some(path) != workspace.config_directory().as_ref())
                            .expect("fixture has another project");
                        workspace.choose_draft_project(&directory, window, cx);
                    });
                }
            })
            .expect("test window");
            if runtime {
                retarget(&workspace, &second, cx);
            }
            settle(window, cx);
            act(window, &page, cx, |page, window, cx| {
                page.open_editor(
                    None,
                    None,
                    Form {
                        name: "new-owner".into(),
                        content: "New content".into(),
                        ..Form::default()
                    },
                    window,
                    cx,
                );
                page.selected = Some("new-owner".into());
            });
            cx.read(|cx| {
                let page = page.read(cx);
                assert_eq!(page.editor.as_ref().expect("new editor").saved_name, None);
                assert_eq!(page.form(cx).expect("new form").content, "New content");
            });
        }
    }

    #[gpui_kit::test]
    fn the_list_groups_by_scope_and_a_saved_edit_patches_the_snippet(cx: &mut TestAppContext) {
        let (server, store) = server(false);
        let (window, workspace) = open(&server, Page::Snippets, cx);
        let page = page(&workspace, cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("snippets-row-plan").is_some());
            assert!(window.try_find("snippets-row-review").is_some());
        })
        .expect("test window");
        act(window, &page, cx, |page, window, cx| {
            page.select("review".into(), window, cx);
        });
        act(window, &page, cx, |page, window, cx| {
            assert!(!page.has_unsaved_changes(cx));
            set_content(page, "Review this carefully", window, cx);
            assert!(page.has_unsaved_changes(cx));
            page.save(window, cx);
        });
        let writes = server.writes();
        assert_eq!(writes.len(), 1);
        assert!(writes[0].is("PATCH", "/api/config/snippets/review"));
        assert_eq!(
            writes[0].body,
            json!({"content": "Review this carefully", "aliases": [], "description": ""}),
            "an unchanged name and scope are not sent"
        );
        assert_eq!(
            store.lock().expect("store")[0]["content"],
            "Review this carefully"
        );
        cx.read(|cx| {
            let page = page.read(cx);
            assert!(
                !page.has_unsaved_changes(cx),
                "the saved form is the new baseline"
            );
            assert_eq!(page.notice.as_deref(), Some("Snippet saved"));
        });
    }

    #[gpui_kit::test]
    fn a_draft_is_checked_before_it_is_created(cx: &mut TestAppContext) {
        let (server, store) = server(false);
        let (window, workspace) = open(&server, Page::Snippets, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.create_new(window, cx)
        });
        act(window, &page, cx, |page, window, cx| {
            assert_eq!(page.selected.as_deref(), Some("new-snippet"));
            page.save(window, cx);
        });
        assert!(
            server.writes().is_empty(),
            "an empty body is refused locally"
        );
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).notice.as_deref(),
                Some("Snippet content is required")
            );
        });
        act(window, &page, cx, |page, window, cx| {
            let name = page.editor.as_ref().expect("editor").name.clone();
            name.update(cx, |input, cx| input.set_value("Review", window, cx));
            set_content(page, "Body", window, cx);
            page.save(window, cx);
        });
        assert!(
            server.writes().is_empty(),
            "a taken name is refused locally"
        );
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).notice.as_deref(),
                Some("#Review is already used by another snippet")
            );
        });
        act(window, &page, cx, |page, window, cx| {
            let name = page.editor.as_ref().expect("editor").name.clone();
            name.update(cx, |input, cx| {
                input.set_value("fresh one", window, cx);
                // Typing reports a change; a programmatic value does not.
                cx.emit(InputEvent::Change);
            });
        });
        act(window, &page, cx, |page, window, cx| {
            assert_eq!(
                page.form(cx).expect("form").name,
                "fresh-one",
                "names are normalized"
            );
            page.save(window, cx);
        });
        let writes = server.writes();
        assert_eq!(writes.len(), 1);
        assert!(writes[0].is("POST", "/api/config/snippets/fresh-one"));
        assert_eq!(writes[0].body["scope"], "global");
        assert_eq!(store.lock().expect("store").len(), 3);
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.selected.as_deref(), Some("fresh-one"));
            assert_eq!(
                page.editor
                    .as_ref()
                    .and_then(|editor| editor.file.as_deref()),
                Some("/global/fresh-one.md"),
                "the created snippet opens on its file"
            );
        });
    }

    #[gpui_kit::test]
    fn unsaved_edits_hold_a_switch_and_delete_clears_the_selection(cx: &mut TestAppContext) {
        let (server, store) = server(false);
        let (window, workspace) = open(&server, Page::Snippets, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.select("plan".into(), window, cx)
        });
        act(window, &page, cx, |page, window, cx| {
            set_content(page, "Edited", window, cx);
            page.select("review".into(), window, cx);
        });
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).selected.as_deref(),
                Some("plan"),
                "the switch waits for the discard answer"
            );
        });
        act(window, &page, cx, |page, window, cx| {
            page.set_mode(ContentMode::Preview, cx);
            let _ = window;
        });
        cx.read(|cx| {
            assert_eq!(
                page.read(cx)
                    .editor
                    .as_ref()
                    .map(|editor| editor.preview.clone()),
                Some(Preview::Ready("<Edited>".into())),
                "the preview expands the unsaved body"
            );
        });
        act(window, &page, cx, |page, window, cx| {
            page.delete("plan".into(), window, cx)
        });
        assert!(
            server
                .writes()
                .iter()
                .any(|write| write.is("DELETE", "/api/config/snippets/plan"))
        );
        assert_eq!(store.lock().expect("store").len(), 1);
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.selected, None);
            assert!(page.editor.is_none());
            assert_eq!(page.snippets().len(), 1, "the list is read again");
        });
    }

    #[gpui_kit::test]
    fn a_failed_list_read_offers_retry(cx: &mut TestAppContext) {
        let (server, _) = server(true);
        let (window, workspace) = open(&server, Page::Snippets, cx);
        let page = page(&workspace, cx);
        cx.read(|cx| assert_eq!(page.read(cx).list, Load::Failed));
        act(window, &page, cx, |page, window, cx| {
            page.create_new(window, cx)
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("snippets-load-failed-retry").is_some());
            assert!(
                window.try_find("snippets-editor").is_none(),
                "no draft without a list"
            );
        })
        .expect("test window");
    }
}
