//! Settings → Commands (`CommandsSidebar.tsx`, `CommandsPage.tsx`): the slash
//! commands of the current directory and one command's editor.
//!
//! An existing command autosaves: the agent, model and child-session controls
//! write at once, text writes when it loses focus, and only the difference
//! from the last saved form is sent. A new command exists only once Create
//! confirms it, so an abandoned draft never reaches disk. Selecting another
//! command first sends the open one's pending text; if that write fails after
//! the switch, the failure is reported and the left edit is not kept, as the
//! reference's page reseeds its form from the newly selected command.
//!
//! Every form that differs from the last one submitted becomes its own
//! accepted write at once. `SettingsWrites` runs a runtime's writes in
//! acceptance order, so an edit made while another write is in flight is
//! already in custody when Settings closes, the app quits or another command
//! is selected. A command with a delete queued accepts no save, so no write
//! can recreate it behind the delete.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::IndexPath;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Theme, WindowExt};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, WeakEntity, Window,
    div, prelude::FluentBuilder, px,
};
use openchamber_native_transport::commands::{
    Command, CommandChanges, CommandError, CommandFileScope, format_model_selection,
    parse_model_selection,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::network::{Network, NetworkTask};
use crate::settings_controls::{
    Load, ServerTarget, SettingsWrites, checkbox_row, confirm, field_row, focus_handle, helper,
    load_failed, same_target,
};
use crate::settings_navigation::Page;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// Everything the page writes into a command's file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Form {
    description: String,
    agent: String,
    /// `provider/model`, or empty for no override.
    model: String,
    variant: String,
    subagent: bool,
    template: String,
}

impl Form {
    fn of(command: &Command) -> Self {
        let (model, variant) = command
            .model
            .as_deref()
            .and_then(parse_model_selection)
            .unwrap_or_default();
        Self {
            description: command.description.clone().unwrap_or_default(),
            agent: command.agent.clone().unwrap_or_default(),
            model,
            variant: variant.unwrap_or_default(),
            subagent: command.subagent,
            template: command.template.clone().unwrap_or_default(),
        }
    }

    /// The form as a save compares and writes it: text trimmed.
    fn trimmed(&self) -> Self {
        Self {
            description: self.description.trim().to_owned(),
            agent: self.agent.trim().to_owned(),
            model: self.model.trim().to_owned(),
            variant: self.variant.trim().to_owned(),
            subagent: self.subagent,
            template: self.template.trim().to_owned(),
        }
    }

    /// The configuration a write sends (`buildConfig`). An empty description
    /// is left as stored; an empty agent or model clears the override.
    fn changes(&self) -> CommandChanges {
        let form = self.trimmed();
        CommandChanges {
            description: (!form.description.is_empty()).then_some(form.description),
            agent: Some((!form.agent.is_empty()).then_some(form.agent)),
            model: Some(format_model_selection(&form.model, &form.variant)),
            template: Some(form.template),
            subagent: Some(form.subagent),
        }
    }

    /// The baseline after `changes` landed, as the reference keeps it: the
    /// trimmed form, with a variant that names no model dropped. An emptied
    /// description stays empty here although the file keeps its old one.
    fn written(&self) -> Self {
        let mut form = self.trimmed();
        if format_model_selection(&form.model, &form.variant).is_none() {
            form.model.clear();
            form.variant.clear();
        }
        form
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Choice {
    value: String,
    label: SharedString,
}

impl SearchableListItem for Choice {
    type Value = String;
    fn title(&self) -> SharedString {
        self.label.clone()
    }
    fn value(&self) -> &String {
        &self.value
    }
}

type Picker = Entity<SelectState<SearchableVec<Choice>>>;

/// One open editor: a saved command, or a draft Create has not confirmed.
struct Editor {
    generation: u64,
    /// `None` for a draft.
    saved_name: Option<String>,
    /// What the command file holds; a save writes only a difference from it.
    saved: Form,
    path: Option<String>,
    legacy: bool,
    scope: CommandFileScope,
    agent: String,
    model: String,
    subagent: bool,
    name: Entity<InputState>,
    description: Entity<TextareaState>,
    variant: Entity<InputState>,
    template: Entity<TextareaState>,
    scope_picker: Picker,
    agent_picker: Picker,
    model_picker: Picker,
    /// The newest form already handed to `SettingsWrites` and not answered
    /// yet. A request compares against it, so one form is accepted once.
    submitted: Option<Form>,
    /// Counts accepted saves; only the newest one's answer settles
    /// `submitted` and the failure flag.
    revision: u64,
    /// Create is in flight.
    creating: bool,
    _subscriptions: Vec<Subscription>,
}

pub(crate) struct CommandsPage {
    owner: WeakEntity<Workspace>,
    network: Option<Network>,
    target: Option<ServerTarget>,
    directory: Option<String>,
    list: Load<Vec<Command>>,
    /// The selected command's name, or the draft's.
    selected: Option<String>,
    editor: Option<Editor>,
    editor_generation: u64,
    read_revision: u64,
    read: Option<Task<()>>,
    writes: SettingsWrites,
    save_failed: bool,
    /// Commands with a delete accepted and not answered. They take no save
    /// and no second delete.
    deleting: HashSet<String>,
    /// Whether Settings shows this page. A hidden page reads nothing.
    shown: bool,
    /// Retry was pressed; the next render sends the kept edit again.
    retry_requested: bool,
    /// Feedback of the last action, kept for tests.
    notice: Option<SharedString>,
    list_scroll: ScrollHandle,
    scroll: ScrollHandle,
    focus: HashMap<SharedString, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

/// The directory commands are read for, as `getRequestDirectory` resolves it.
fn directory(owner: &WeakEntity<Workspace>, cx: &App) -> Option<String> {
    owner.upgrade()?.read(cx).config_directory()
}

/// Whether a command file may carry `name` once whitespace became dashes.
fn command_name(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join("-")
}

impl CommandsPage {
    pub(crate) fn new(owner: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(workspace) = owner.upgrade() {
            subscriptions.push(cx.observe(&workspace, |this, _, cx| {
                let target = ServerTarget::capture(&this.owner, this.network.as_ref(), cx);
                let moved = !same_target(this.target.as_ref(), target.as_ref())
                    || this.directory != directory(&this.owner, cx);
                if !moved {
                    return;
                }
                if this.shown {
                    this.refresh(cx);
                } else if this.target.is_some() {
                    // Hidden: the old owner's list and editor are let go
                    // without a read. Showing the page reads the current one.
                    this.retire();
                    this.target = None;
                    this.read = None;
                    cx.notify();
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
            deleting: HashSet::new(),
            shown: false,
            retry_requested: false,
            notice: None,
            list_scroll: ScrollHandle::new(),
            scroll: ScrollHandle::new(),
            focus: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Settings shows or leaves this page. Showing it reads again.
    pub(crate) fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.shown = shown;
        if shown {
            self.refresh(cx);
        }
    }

    /// Lets go of everything that belongs to the server and directory shown
    /// so far. Writes already accepted for it still run to their answer.
    fn retire(&mut self) {
        self.list = Load::Loading;
        self.selected = None;
        self.editor = None;
        self.save_failed = false;
        self.deleting.clear();
    }

    /// Reads the list for the current server and directory.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let target = ServerTarget::capture(&self.owner, self.network.as_ref(), cx);
        let directory = directory(&self.owner, cx);
        if !same_target(self.target.as_ref(), target.as_ref()) || self.directory != directory {
            // Another server's or project's commands never carry over.
            self.retire();
        }
        self.target = target;
        self.directory = directory;
        if self.list == Load::Failed {
            self.list = Load::Loading;
        }
        self.read_list(cx);
    }

    /// Reads the list. A list already shown stays through a failed reread; a
    /// first read fails visibly and never shows as an empty list.
    fn read_list(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            self.read = None;
            cx.notify();
            return;
        };
        let directory = self.directory.clone();
        self.read_revision += 1;
        let revision = self.read_revision;
        let client = target.client;
        let reading = NetworkTask::spawn_on(&target.network, async move {
            let (commands, skills) = tokio::join!(
                client.commands(directory.as_deref()),
                client.skill_names(directory.as_deref())
            );
            // OpenCode lists skills among its commands; Settings edits them on
            // the Skills page. A failed skills read filters nothing.
            let skills = skills.unwrap_or_default();
            commands.map(|commands| {
                commands
                    .into_iter()
                    .filter(|command| !skills.contains(&command.name))
                    .collect::<Vec<_>>()
            })
        });
        self.read = Some(cx.spawn(async move |this, cx| {
            let result = reading.await;
            let _ = this.update(cx, |this, cx| {
                if this.read_revision != revision {
                    return;
                }
                this.read = None;
                match result {
                    Some(Ok(commands)) => {
                        if let Some(editor) = &mut this.editor
                            && let Some(command) = commands
                                .iter()
                                .find(|command| Some(&command.name) == editor.saved_name.as_ref())
                        {
                            editor.path = command.path.clone();
                            editor.legacy = command.legacy;
                        }
                        this.list = Load::Ready(commands);
                    }
                    _ if this.list.ready().is_some() => {}
                    _ => this.list = Load::Failed,
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn commands(&self) -> &[Command] {
        self.list.ready().map_or(&[], Vec::as_slice)
    }

    fn form(&self, cx: &App) -> Option<Form> {
        let editor = self.editor.as_ref()?;
        Some(Form {
            description: editor.description.read(cx).value().to_string(),
            agent: editor.agent.clone(),
            model: editor.model.clone(),
            variant: editor.variant.read(cx).value().to_string(),
            subagent: editor.subagent,
            template: editor.template.read(cx).value().to_string(),
        })
    }

    fn create_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.list.ready().is_none() {
            return;
        }
        self.open_editor(None, Form::default(), None, false, window, cx);
        self.selected = Some(String::new());
    }

    fn select(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(name) && self.editor.is_some() {
            return;
        }
        let Some(command) = self
            .commands()
            .iter()
            .find(|command| command.name == name)
            .cloned()
        else {
            return;
        };
        // Text still focused in the editor being left is committed first.
        self.request_save(window, cx);
        self.open_editor(
            Some(command.name.clone()),
            Form::of(&command),
            command.path.clone(),
            command.legacy,
            window,
            cx,
        );
        self.selected = Some(command.name);
    }

    /// The agents and models the pickers offer: "not selected", the current
    /// directory's catalog, and the stored value when the catalog lacks it.
    fn choices(&self, form: &Form, cx: &App) -> (Vec<Choice>, Vec<Choice>) {
        let none = Choice {
            value: String::new(),
            label: locale::strings(cx).get("settings.commands.agentSelector.notSelected"),
        };
        let (mut agents, mut models) = (vec![none.clone()], vec![none]);
        let catalog = self.owner.upgrade().and_then(|owner| {
            let workspace = owner.read(cx);
            let directory = self.directory.as_deref()?;
            let catalog = workspace.send_catalog.directory(directory)?;
            Some((
                catalog.agents.ready().cloned().unwrap_or_default(),
                catalog.providers.ready().cloned().unwrap_or_default(),
            ))
        });
        if let Some((catalog_agents, providers)) = catalog {
            for agent in catalog_agents.iter().filter(|agent| !agent.hidden) {
                agents.push(Choice {
                    value: agent.name.clone(),
                    label: agent
                        .display_name
                        .clone()
                        .unwrap_or_else(|| agent.name.clone())
                        .into(),
                });
            }
            for provider in &providers {
                let provider_name = provider.name.as_deref().unwrap_or(&provider.id);
                for model in &provider.models {
                    models.push(Choice {
                        value: format!("{}/{}", provider.id, model.id),
                        label: format!(
                            "{provider_name} / {}",
                            model.name.as_deref().unwrap_or(&model.id)
                        )
                        .into(),
                    });
                }
            }
        }
        for (list, value) in [(&mut agents, &form.agent), (&mut models, &form.model)] {
            if !list.iter().any(|choice| &choice.value == value) {
                list.push(Choice {
                    value: value.clone(),
                    label: value.clone().into(),
                });
            }
        }
        (agents, models)
    }

    fn open_editor(
        &mut self,
        saved_name: Option<String>,
        form: Form,
        path: Option<String>,
        legacy: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let strings = locale::strings(cx).clone();
        let input =
            |value: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
                let (value, placeholder) = (value.to_owned(), strings.get(placeholder));
                cx.new(|cx| {
                    let mut state = InputState::new(window, cx);
                    state.set_placeholder(placeholder, window, cx);
                    state.set_value(value, window, cx);
                    state
                })
            };
        let name = input(
            "",
            "settings.commands.page.field.commandNamePlaceholder",
            window,
            cx,
        );
        let variant = input(
            &form.variant,
            "settings.agents.page.field.variantPlaceholder",
            window,
            cx,
        );
        let description = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .auto_grow(2, 6)
                .placeholder(strings.get("settings.commands.page.field.descriptionPlaceholder"));
            state.set_value(form.description.clone(), window, cx);
            state
        });
        let template = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .auto_grow(12, 40)
                .placeholder(strings.get("settings.commands.page.field.templatePlaceholder"));
            state.set_value(form.template.clone(), window, cx);
            state
        });
        let picker = |choices: Vec<Choice>,
                      current: &str,
                      window: &mut Window,
                      cx: &mut Context<Self>|
         -> Picker {
            let selected = choices
                .iter()
                .position(|choice| choice.value == current)
                .unwrap_or(0);
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(choices),
                    Some(IndexPath::new(selected)),
                    window,
                    cx,
                )
            })
        };
        let (agents, models) = self.choices(&form, cx);
        let agent_picker = picker(agents, &form.agent, window, cx);
        let model_picker = picker(models, &form.model, window, cx);
        let scope_picker = picker(
            vec![
                Choice {
                    value: "user".into(),
                    label: strings.get("settings.common.scope.global"),
                },
                Choice {
                    value: "project".into(),
                    label: strings.get("settings.common.scope.project"),
                },
            ],
            "user",
            window,
            cx,
        );
        let subscriptions = vec![
            cx.subscribe_in(&name, window, |_, _, _: &InputEvent, _, cx| cx.notify()),
            cx.subscribe_in(&description, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Blur) {
                    this.request_save(window, cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&variant, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Blur) {
                    this.request_save(window, cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&template, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Blur) {
                    this.request_save(window, cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&agent_picker, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(value)) = event
                    && let Some(editor) = &mut this.editor
                {
                    editor.agent = value.clone();
                    this.request_save(window, cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&model_picker, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(value)) = event
                    && let Some(editor) = &mut this.editor
                {
                    editor.model = value.clone();
                    this.request_save(window, cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&scope_picker, window, |this, _, event, _, cx| {
                if let SelectEvent::Confirm(Some(value)) = event
                    && let Some(editor) = &mut this.editor
                {
                    editor.scope = if value == "project" {
                        CommandFileScope::Project
                    } else {
                        CommandFileScope::User
                    };
                    cx.notify();
                }
            }),
        ];
        self.editor_generation += 1;
        self.save_failed = false;
        self.editor = Some(Editor {
            generation: self.editor_generation,
            saved_name,
            path,
            legacy,
            scope: CommandFileScope::User,
            agent: form.agent.clone(),
            model: form.model.clone(),
            subagent: form.subagent,
            saved: form,
            name,
            description,
            variant,
            template,
            scope_picker,
            agent_picker,
            model_picker,
            submitted: None,
            revision: 0,
            creating: false,
            _subscriptions: subscriptions,
        });
        self.scroll.set_offset(gpui_kit::point(px(0.0), px(0.0)));
        cx.notify();
    }

    fn set_subagent(&mut self, subagent: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.editor {
            editor.subagent = subagent;
            self.request_save(window, cx);
            cx.notify();
        }
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

    /// Whether a completion still belongs to the connection and directory
    /// that started it.
    fn current(&self, target: &ServerTarget, directory: &Option<String>, cx: &App) -> bool {
        same_target(self.target.as_ref(), Some(target))
            && same_target(
                ServerTarget::capture(&self.owner, self.network.as_ref(), cx).as_ref(),
                Some(target),
            )
            && &self.directory == directory
    }

    /// Autosave of an existing command (`save`). A form that differs from
    /// the last one submitted is accepted as a write of its own at once, also
    /// while an earlier one is in flight; the server gets them in order. A
    /// failed write keeps the edit, and the next request or Retry sends it
    /// again. A command with a delete queued accepts nothing.
    pub(crate) fn request_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() {
            return;
        }
        let (Some(form), Some(target)) = (self.form(cx), self.target.clone()) else {
            return;
        };
        let Some(editor) = &mut self.editor else {
            return;
        };
        let Some(name) = editor.saved_name.clone() else {
            return;
        };
        if self.deleting.contains(&name) {
            return;
        }
        let form = form.trimmed();
        let in_flight = editor.submitted.is_some();
        let baseline = editor
            .submitted
            .clone()
            .unwrap_or_else(|| editor.saved.trimmed());
        if form == baseline {
            if !in_flight {
                self.save_failed = false;
            }
            return;
        }
        if form.template.is_empty() {
            self.save_failed = true;
            let text = locale::strings(cx).get("settings.commands.page.toast.templateRequired");
            self.toast(text, false, window, cx);
            return;
        }
        editor.revision += 1;
        let (generation, revision) = (editor.generation, editor.revision);
        let written = form.written();
        editor.submitted = Some(form.clone());
        let changes = form.changes();
        let directory = self.directory.clone();
        let client = target.client.clone();
        let write_directory = directory.clone();
        let (id, writing) = self.writes.submit(&target, async move {
            client
                .update_command(&name, &changes, write_directory.as_deref())
                .await
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<(), CommandError>> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if !this.current(&target, &directory, cx) {
                    return;
                }
                let saved = matches!(result, Some(Ok(())));
                if saved {
                    this.read_list(cx);
                }
                // An editor left meanwhile took its edit with it, as in the
                // reference; its failed write is still reported below.
                if let Some(editor) = this
                    .editor
                    .as_mut()
                    .filter(|editor| editor.generation == generation)
                {
                    if saved {
                        editor.saved = written;
                    }
                    // Only the newest accepted form settles the editor: an
                    // older answer leaves the newer one in flight.
                    if editor.revision == revision {
                        editor.submitted = None;
                        this.save_failed = !saved;
                    }
                }
                if !saved {
                    let text = locale::strings(cx).get("settings.commands.page.toast.updateFailed");
                    this.toast(text, false, window, cx);
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    /// Creates the draft (`handleCreate`).
    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() {
            return;
        }
        let (Some(form), Some(target)) = (self.form(cx), self.target.clone()) else {
            return;
        };
        let Some(editor) = &self.editor else {
            return;
        };
        if editor.saved_name.is_some() || editor.creating {
            return;
        }
        let name = command_name(&editor.name.read(cx).value());
        let strings = locale::strings(cx).clone();
        let refusal = if name.is_empty() {
            Some("settings.commands.sidebar.toast.commandNameRequired")
        } else if form.template.trim().is_empty() {
            Some("settings.commands.page.toast.templateRequired")
        } else if self.commands().iter().any(|command| command.name == name) {
            Some("settings.commands.sidebar.toast.commandExists")
        } else {
            None
        };
        if let Some(key) = refusal {
            self.toast(strings.get(key), false, window, cx);
            return;
        }
        let editor = self.editor.as_mut().expect("checked above");
        editor.creating = true;
        let generation = editor.generation;
        let scope = editor.scope;
        let written = form.trimmed();
        let changes = form.changes();
        let directory = self.directory.clone();
        let client = target.client.clone();
        let (write_directory, write_name) = (directory.clone(), name.clone());
        let (id, writing) = self.writes.submit(&target, async move {
            client
                .create_command(&write_name, &changes, scope, write_directory.as_deref())
                .await
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<(), CommandError>> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if !this.current(&target, &directory, cx) {
                    return;
                }
                let created = matches!(result, Some(Ok(())));
                if created {
                    this.read_list(cx);
                }
                let strings = locale::strings(cx).clone();
                if let Some(editor) = this
                    .editor
                    .as_mut()
                    .filter(|editor| editor.generation == generation)
                {
                    editor.creating = false;
                    this.save_failed = !created;
                    if created {
                        // The draft is now the saved command it created.
                        editor.saved_name = Some(name.clone());
                        editor.saved = written;
                        this.selected = Some(name.clone());
                    }
                }
                this.toast(
                    strings.get(if created {
                        "settings.commands.page.toast.created"
                    } else {
                        "settings.commands.page.toast.createFailed"
                    }),
                    created,
                    window,
                    cx,
                );
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    fn cancel_create(&mut self, cx: &mut Context<Self>) {
        self.editor = None;
        self.selected = None;
        self.save_failed = false;
        cx.notify();
    }

    /// Asks before deleting a custom command or resetting a built-in one.
    fn ask_remove(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let strings = locale::strings(cx);
        let built_in = self
            .commands()
            .iter()
            .any(|command| command.name == name && command.is_built_in());
        let (title, description, action) = if built_in {
            (
                "settings.commands.sidebar.dialog.resetTitle",
                "settings.commands.sidebar.dialog.resetDescription",
                "settings.common.actions.reset",
            )
        } else {
            (
                "settings.commands.sidebar.dialog.deleteTitle",
                "settings.commands.sidebar.dialog.deleteDescription",
                "settings.common.actions.delete",
            )
        };
        confirm(
            cx.weak_entity(),
            strings.get(title),
            strings.format(description, &[("name", &name)]),
            strings.get(action),
            window,
            cx,
            move |this, window, cx| this.remove(name.clone(), built_in, window, cx),
        );
    }

    /// Deletes a command, or resets a built-in one. From here until the
    /// answer the command takes no save and no second delete: a save queued
    /// behind the delete would write the file again.
    fn remove(&mut self, name: String, reset: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() || self.deleting.contains(&name) {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        self.deleting.insert(name.clone());
        let directory = self.directory.clone();
        let client = target.client.clone();
        let (write_directory, removed) = (directory.clone(), name.clone());
        let (id, writing) = self.writes.submit(&target, async move {
            client
                .delete_command(&removed, write_directory.as_deref())
                .await
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result: Option<Result<(), CommandError>> = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if !this.current(&target, &directory, cx) {
                    return;
                }
                this.deleting.remove(&name);
                let strings = locale::strings(cx).clone();
                if matches!(result, Some(Ok(()))) {
                    if this.selected.as_deref() == Some(name.as_str()) {
                        this.selected = None;
                        this.editor = None;
                        this.save_failed = false;
                    }
                    let key = if reset {
                        "settings.commands.sidebar.toast.commandReset"
                    } else {
                        "settings.commands.sidebar.toast.commandDeleted"
                    };
                    this.toast(strings.format(key, &[("name", &name)]), true, window, cx);
                    this.read_list(cx);
                } else {
                    let key = if reset {
                        "settings.commands.sidebar.toast.resetFailed"
                    } else {
                        "settings.commands.sidebar.toast.deleteFailed"
                    };
                    this.toast(strings.get(key), false, window, cx);
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
    }

    /// Commits text still held by a focused field before writers freeze. It
    /// is accepted even while an earlier write is in flight.
    pub(crate) fn prepare_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.request_save(window, cx);
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

    /// `commands.create` starts a draft; the field results focus their
    /// control in the open editor.
    pub(crate) fn reveal(&mut self, item: &str, window: &mut Window, cx: &mut Context<Self>) {
        if item == "commands.create" || (item == "commands.name" && self.editor.is_none()) {
            self.create_new(window, cx);
        }
        let Some(editor) = &self.editor else {
            return;
        };
        match item {
            "commands.name" if editor.saved_name.is_none() => {
                let name = editor.name.clone();
                name.update(cx, |input, cx| input.focus(window, cx));
            }
            "commands.variant" => {
                let variant = editor.variant.clone();
                variant.update(cx, |input, cx| input.focus(window, cx));
            }
            "commands.template" => {
                let template = editor.template.clone();
                template.update(cx, |input, cx| input.focus(window, cx));
            }
            _ => {}
        }
    }
}

impl CommandsPage {
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

    fn button(
        &mut self,
        id: &'static str,
        label: SharedString,
        enabled: bool,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        on_activate(
            gpui_kit::base::Button::new(id)
                .accessibility_id(id)
                .accessibility_label(label.clone())
                .disabled(!enabled)
                .h(gpui_kit::rems(1.75))
                .px(px(8.0))
                .rounded(px(7.0))
                .child(label),
            cx,
            action,
        )
        .into_any_element()
    }

    fn row(&mut self, command: &Command, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let selected = self.selected.as_deref() == Some(command.name.as_str());
        let id: SharedString = format!("commands-row-{}", command.name).into();
        let focus = focus_handle(&mut self.focus, &id, cx);
        let built_in = command.is_built_in();
        let badge: Option<SharedString> = if built_in {
            Some(strings.get("settings.agents.sidebar.badge.system"))
        } else {
            command.scope.map(|scope| match scope {
                CommandFileScope::User => "user".into(),
                CommandFileScope::Project => "project".into(),
            })
        };
        let name = command.name.clone();
        let removed = command.name.clone();
        let remove = self.icon_button(
            format!("commands-remove-{}", command.name).into(),
            if built_in { "restart" } else { "delete-bin" },
            strings.get(if built_in {
                "settings.common.actions.reset"
            } else {
                "settings.common.actions.delete"
            }),
            cx,
            move |this, window, cx| this.ask_remove(removed.clone(), window, cx),
        );
        let label: SharedString = format!("/{}", command.name).into();
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
                        .aria_label(label.clone())
                        .aria_selected(selected)
                        .track_focus(&focus)
                        .tab_index(0)
                        .flex_1()
                        .min_w_0()
                        .cursor_pointer()
                        .child(
                            div()
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(6.0))
                                .child(div().truncate().child(label))
                                .children(badge.map(|badge| {
                                    div()
                                        .flex_shrink_0()
                                        .px(px(4.0))
                                        .rounded(px(4.0))
                                        .bg(palette.elevated)
                                        .css_text(0.6875, 1.0, cx)
                                        .text_color(palette.muted_foreground)
                                        .child(badge)
                                })),
                        )
                        .children(command.description.clone().map(|description| {
                            div()
                                .truncate()
                                .css_text(0.6875, 1.0, cx)
                                .text_color(palette.muted_foreground)
                                .child(description)
                        })),
                    cx,
                    move |this, window, cx| this.select(&name, window, cx),
                )
                .test_support(),
            )
            .child(remove)
            .into_any_element()
    }

    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let count = self.commands().len().to_string();
        let create = self.icon_button(
            "commands-create".into(),
            "add",
            strings.get("settings.commands.page.title.new"),
            cx,
            |this, window, cx| this.create_new(window, cx),
        );
        let mut sorted: Vec<Command> = self.commands().to_vec();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        let mut list = div()
            .id("commands-list")
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
                    "commands-load-failed",
                    strings.get("settings.openchamber.desktopNetwork.error.loadFailed"),
                    strings.get("desktopHostSwitcher.actions.retry"),
                    cx,
                    |this, cx| this.refresh(cx),
                ));
            }
            Load::Loading => {}
            Load::Ready(commands) if commands.is_empty() => {
                list = list.child(
                    div()
                        .id("commands-empty")
                        .py(px(48.0))
                        .px(px(16.0))
                        .text_center()
                        .text_color(palette.muted_foreground)
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .child(strings.get("settings.commands.sidebar.empty.title")),
                        )
                        .child(
                            div()
                                .mt(px(4.0))
                                .css_text(0.8125, 1.25, cx)
                                .child(strings.get("settings.commands.sidebar.empty.description")),
                        ),
                );
            }
            Load::Ready(_) => {
                for (built_in, title) in [
                    (true, "settings.commands.sidebar.section.builtIn"),
                    (false, "settings.commands.sidebar.section.custom"),
                ] {
                    let group: Vec<&Command> = sorted
                        .iter()
                        .filter(|command| command.is_built_in() == built_in)
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
                            .child(strings.get(title)),
                    );
                    for command in group {
                        column = column.child(self.row(command, cx));
                    }
                    list = list.child(column);
                }
            }
        }
        div()
            .id("commands-sidebar")
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
                            .id("commands-sidebar-title")
                            .role(Role::Heading)
                            .font_weight(FontWeight::SEMIBOLD)
                            .mb(px(12.0))
                            .child(strings.get("settings.commands.sidebar.title")),
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
                                        "settings.commands.sidebar.total",
                                        &[("count", &count)],
                                    )),
                            )
                            .child(create),
                    ),
            )
            .child(list)
            .into_any_element()
    }

    fn section(&self, title: &'static str, first: bool, cx: &App) -> gpui_kit::Div {
        let palette = cx.global::<Palette>();
        div()
            .when(!first, |section| {
                section
                    .mt(px(32.0))
                    .pt(px(24.0))
                    .border_t_1()
                    .border_color(palette.border)
            })
            .child(
                div()
                    .id(title)
                    .role(Role::Heading)
                    .font_weight(FontWeight::SEMIBOLD)
                    .mb(px(12.0))
                    .child(locale::strings(cx).get(title)),
            )
    }

    fn editor_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let Some(form) = self.form(cx) else {
            return div()
                .id("commands-empty-editor")
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(4.0))
                .text_color(palette.muted_foreground)
                .child(glyph("terminal-box", 48.0, palette.muted_foreground))
                .child(strings.get("settings.commands.page.empty.title"))
                .child(
                    div()
                        .css_text(0.8125, 1.25, cx)
                        .child(strings.get("settings.commands.page.empty.description")),
                )
                .into_any_element();
        };
        let editor = self.editor.as_ref().expect("a form has an editor");
        let is_new = editor.saved_name.is_none();
        let saving = editor.creating;
        let legacy = editor.legacy;
        let subagent = editor.subagent;
        let name_text = editor.name.read(cx).value().to_string();
        let title: SharedString = match &editor.saved_name {
            Some(name) => format!("/{name}").into(),
            None => strings.get("settings.commands.page.title.new"),
        };
        let subtitle = strings.get(if is_new {
            "settings.commands.page.subtitle.new"
        } else {
            "settings.commands.page.subtitle.edit"
        });
        let (name, description, variant, template) = (
            editor.name.clone(),
            editor.description.clone(),
            editor.variant.clone(),
            editor.template.clone(),
        );
        let (scope_picker, agent_picker, model_picker) = (
            editor.scope_picker.clone(),
            editor.agent_picker.clone(),
            editor.model_picker.clone(),
        );
        let closing = self.writes.closing();
        let save_failed = self.save_failed && !is_new;
        let subagent_focus = focus_handle(&mut self.focus, &"commands-subagent".into(), cx);
        let picker = |id: &'static str, state: &Picker, label: SharedString| {
            div()
                .w(px(224.0))
                .key_context("SettingsPicker")
                .child(
                    Select::new(state)
                        .id(id)
                        .accessibility_label(label)
                        .w_full(),
                )
                .into_any_element()
        };
        let mut identity = self.section("settings.commands.page.section.identity", true, cx);
        if is_new {
            identity = identity.child(field_row(
                "commands.name",
                strings.get("settings.commands.page.field.commandName"),
                None,
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(4.0))
                            .child(div().text_color(palette.muted_foreground).child("/"))
                            .child(
                                div().w(px(160.0)).child(
                                    Input::new(&name)
                                        .id("commands-name")
                                        .accessibility_id("commands-name")
                                        .aria_label(
                                            strings.get("settings.commands.page.field.commandName"),
                                        ),
                                ),
                            ),
                    )
                    .child(
                        div().w(px(120.0)).key_context("SettingsPicker").child(
                            Select::new(&scope_picker)
                                .id("commands-scope")
                                .accessibility_label(strings.get("snippets.field.scopeAria"))
                                .w_full(),
                        ),
                    )
                    .into_any_element(),
                None,
                cx,
            ));
        }
        identity = identity.child(
            div()
                .py(px(6.0))
                .child(strings.get("settings.common.field.description"))
                .child(
                    div().mt(px(6.0)).child(
                        Textarea::new(&description)
                            .accessibility_id("commands-description")
                            .aria_label(strings.get("settings.common.field.description")),
                    ),
                ),
        );
        let execution = self
            .section("settings.commands.page.section.executionContext", false, cx)
            .child(field_row(
                "commands.agent",
                strings.get("settings.commands.page.field.overrideAgent"),
                None,
                picker(
                    "commands-agent",
                    &agent_picker,
                    strings.get("settings.commands.page.field.overrideAgent"),
                ),
                None,
                cx,
            ))
            .child(field_row(
                "commands.model",
                strings.get("settings.agents.page.field.overrideModel"),
                None,
                picker(
                    "commands-model",
                    &model_picker,
                    strings.get("settings.agents.page.field.overrideModel"),
                ),
                None,
                cx,
            ))
            .child(field_row(
                "commands.variant",
                strings.get("settings.agents.page.field.variant"),
                Some(strings.get("settings.agents.page.field.variantTooltip")),
                div()
                    .w(px(160.0))
                    // A variant needs a model; one already stored stays editable.
                    .when(form.model.is_empty() && form.variant.is_empty(), |field| {
                        field.opacity(0.5)
                    })
                    .child(
                        Input::new(&variant)
                            .id("commands-variant")
                            .accessibility_id("commands-variant")
                            .disabled(form.model.is_empty() && form.variant.is_empty())
                            .aria_label(strings.get("settings.agents.page.field.variant")),
                    )
                    .into_any_element(),
                None,
                cx,
            ))
            .child(checkbox_row(
                "commands-subagent".into(),
                strings.get("settings.commands.page.field.subagent"),
                strings.get("settings.commands.page.field.subagent"),
                subagent,
                !closing,
                &subagent_focus,
                None,
                cx,
                |this: &mut Self, checked, window, cx| this.set_subagent(checked, window, cx),
            ))
            .child(helper(
                strings.get("settings.commands.page.field.subagentTooltip"),
                cx,
            ));
        let mono = Theme::global(cx).mono_font_family.clone();
        let code = |text: &'static str| {
            div()
                .font_family(mono.clone())
                .text_color(palette.foreground)
                .child(text)
        };
        let mut template_section = self
            .section("settings.commands.page.section.template", false, cx)
            .child(
                div()
                    .css_text(0.8125, 1.25, cx)
                    .font_family(mono.clone())
                    .child(
                        Textarea::new(&template)
                            .accessibility_id("commands-template")
                            .aria_label(strings.get("settings.commands.page.section.template")),
                    ),
            )
            // Required syntax the user reads while typing stays visible.
            .child(
                div()
                    .mt(px(8.0))
                    .flex_row_from(direction)
                    .flex_wrap()
                    .gap(px(4.0))
                    .css_text(0.8125, 1.25, cx)
                    .text_color(palette.muted_foreground)
                    .child(code("$ARGUMENTS"))
                    .child(strings.get("settings.commands.page.templateHint.userInput"))
                    .child("·")
                    .child(code("!`cmd`"))
                    .child(strings.get("settings.commands.page.templateHint.shellOutput"))
                    .child("·")
                    .child(code("@file"))
                    .child(strings.get("settings.commands.page.templateHint.fileContents")),
            );
        if is_new {
            let can_create = !saving
                && !closing
                && !name_text.trim().is_empty()
                && !form.template.trim().is_empty();
            let create = self.button(
                "commands-create-confirm",
                strings.get(if saving {
                    "settings.common.actions.saving"
                } else {
                    "settings.common.actions.create"
                }),
                can_create,
                cx,
                |this, window, cx| this.create(window, cx),
            );
            let cancel = self.button(
                "commands-create-cancel",
                strings.get("settings.common.actions.cancel"),
                !saving,
                cx,
                |this, _, cx| this.cancel_create(cx),
            );
            template_section = template_section.child(
                div()
                    .pt(px(12.0))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .child(create)
                    .child(cancel),
            );
        }
        let failed = save_failed.then(|| {
            load_failed(
                "commands-save-failed",
                strings.get("settings.common.status.saveFailed"),
                strings.get("desktopHostSwitcher.actions.retry"),
                cx,
                |this, cx| {
                    // The next render has the window a write completes in.
                    this.retry_requested = true;
                    cx.notify();
                },
            )
        });
        div()
            .css_text(0.875, 1.25, cx)
            .id("commands-editor")
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
                            .id("commands-title")
                            .accessibility_id("commands-title")
                            .role(Role::Heading)
                            .aria_label(title.clone())
                            .css_text(1.125, 1.5, cx)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_color(palette.muted_foreground)
                            .child(subtitle),
                    ),
            )
            .children((legacy && !is_new).then(|| {
                div()
                    .id("commands-legacy-note")
                    .mb(px(16.0))
                    .flex_row_from(direction)
                    .items_start()
                    .gap(px(6.0))
                    .child(glyph("information", 14.0, palette.muted_foreground))
                    .child(helper(strings.get("settings.common.legacyFormat.note"), cx))
            }))
            .children(failed)
            .child(identity)
            .child(execution)
            .child(template_section)
            .into_any_element()
    }
}

impl Render for CommandsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        if std::mem::take(&mut self.retry_requested) {
            self.request_save(window, cx);
        }
        if self.target.is_none() {
            return div()
                .id("commands-settings-page")
                .size_full()
                .px(px(48.0))
                .py(px(32.0))
                .child(crate::settings_view::page_header(Page::Commands, cx))
                .child(helper(strings.get("common.unavailable"), cx))
                .into_any_element();
        }
        let sidebar = self.sidebar(cx);
        let editor = self.editor_view(cx);
        div()
            .id("commands-settings-page")
            .role(Role::Group)
            .aria_label(strings.get(Page::Commands.title()))
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, TestAppContext};
    use serde_json::{Value, json};

    use super::*;
    use crate::settings_controls::tests::{ResponseGate, retarget};
    use crate::settings_test_server::{Server, open, settings, settle};

    /// A command store: OpenCode's list, each command's stored entry, create,
    /// update and delete. `fail_list` and `fail_writes` answer 500 while set.
    struct Fixture {
        server: Server,
        store: Arc<Mutex<Vec<Value>>>,
        fail_list: Arc<AtomicBool>,
        fail_writes: Arc<AtomicBool>,
    }

    fn entry(name: &str, template: &str) -> Value {
        json!({"name": name, "description": format!("About {name}"), "template": template})
    }

    fn fixture() -> Fixture {
        gated_fixture(None)
    }

    /// `hold` names a method whose first request waits on the gate.
    fn gated_fixture(hold: Option<(&'static str, Arc<ResponseGate>)>) -> Fixture {
        let held_once = AtomicBool::new(false);
        let store = Arc::new(Mutex::new(vec![
            entry("review", "Review the diff"),
            entry("ship", "Ship $ARGUMENTS"),
        ]));
        let fail_list = Arc::new(AtomicBool::new(false));
        let fail_writes = Arc::new(AtomicBool::new(false));
        let (held, list_fails, write_fails) = (
            Arc::clone(&store),
            Arc::clone(&fail_list),
            Arc::clone(&fail_writes),
        );
        let server = Server::new(move |request| {
            if let Some((method, gate)) = &hold
                && request.method == *method
                && !held_once.swap(true, Ordering::AcqRel)
            {
                gate.hold();
            }
            let path = request.target.split('?').next().unwrap_or_default();
            let mut store = held.lock().expect("store");
            if request.is("GET", "/api/command") {
                if list_fails.load(Ordering::Acquire) {
                    return (500, json!({"error": "fixture read failure"}));
                }
                let listed: Vec<Value> = store
                    .iter()
                    .map(|command| json!({"name": command["name"], "description": command["description"]}))
                    .collect();
                // OpenCode's list routes answer `{ location, data }`.
                return (200, json!({"location": {}, "data": listed}));
            }
            if request.is("GET", "/api/config/skills") {
                return (200, json!({"skills": [{"name": "a-skill"}]}));
            }
            let Some(rest) = path.strip_prefix("/api/config/commands/") else {
                return (404, json!({"error": "not in this fixture"}));
            };
            let name = rest.trim_end_matches("/config").to_owned();
            if request.method == "GET" {
                let Some(command) = store
                    .iter()
                    .find(|command| command["name"] == name.as_str())
                else {
                    return (404, json!({"error": "not found"}));
                };
                return (
                    200,
                    json!({
                        "source": "md",
                        "scope": "user",
                        "path": format!("/config/command/{name}.md"),
                        "legacy": false,
                        "config": command,
                    }),
                );
            }
            if write_fails.load(Ordering::Acquire) {
                return (500, json!({"error": "fixture write failure"}));
            }
            match request.method.as_str() {
                "POST" => {
                    let mut command = request.body.clone();
                    command["name"] = json!(name);
                    store.push(command);
                }
                "PATCH" => {
                    let Some(command) = store
                        .iter_mut()
                        .find(|command| command["name"] == name.as_str())
                    else {
                        return (404, json!({"error": "not found"}));
                    };
                    if let Some(changes) = request.body.as_object() {
                        for (key, value) in changes {
                            command[key.as_str()] = value.clone();
                        }
                    }
                }
                "DELETE" => store.retain(|command| command["name"] != name.as_str()),
                _ => return (404, json!({"error": "not in this fixture"})),
            }
            (200, json!({"success": true}))
        });
        Fixture {
            server,
            store,
            fail_list,
            fail_writes,
        }
    }

    fn page(workspace: &Entity<Workspace>, cx: &TestAppContext) -> Entity<CommandsPage> {
        let settings = settings(workspace, cx);
        cx.read(|cx| settings.read(cx).commands.clone())
    }

    fn act(
        window: AnyWindowHandle,
        page: &Entity<CommandsPage>,
        cx: &mut TestAppContext,
        action: impl FnOnce(&mut CommandsPage, &mut Window, &mut Context<CommandsPage>),
    ) {
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| action(page, window, cx));
        })
        .expect("test window");
        settle(window, cx);
    }

    fn set_template(
        page: &mut CommandsPage,
        text: &str,
        window: &mut Window,
        cx: &mut Context<CommandsPage>,
    ) {
        let template = page.editor.as_ref().expect("editor").template.clone();
        let text = text.to_owned();
        template.update(cx, |input, cx| input.set_value(text, window, cx));
    }

    fn writes(fixture: &Fixture) -> Vec<crate::settings_test_server::Request> {
        fixture.server.writes()
    }

    #[test]
    fn a_save_writes_trimmed_text_and_clears_an_emptied_override() {
        let form = Form {
            description: "  ".into(),
            agent: String::new(),
            model: String::new(),
            variant: "high".into(),
            subagent: true,
            template: " Body 2 \n".into(),
        };
        let changes = form.changes();
        assert_eq!(changes.description, None, "an empty description is left");
        assert_eq!(changes.agent, Some(None));
        assert_eq!(changes.model, Some(None), "a variant alone names no model");
        assert_eq!(changes.template.as_deref(), Some("Body 2"));
        let written = form.written();
        assert_eq!(written.description, "");
        assert_eq!(written.variant, "");
        assert_eq!(command_name("  my  new command "), "my-new-command");
    }

    #[gpui_kit::test]
    fn the_list_loads_grouped_and_a_selected_command_opens_its_stored_form(
        cx: &mut TestAppContext,
    ) {
        let fixture = fixture();
        let (window, workspace) = open(&fixture.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("commands-row-review").is_some());
            assert!(window.try_find("commands-row-ship").is_some());
            assert!(window.try_find("commands-load-failed").is_none());
        })
        .expect("test window");
        act(window, &page, cx, |page, window, cx| {
            page.select("ship", window, cx)
        });
        cx.read(|cx| {
            let page = page.read(cx);
            let form = page.form(cx).expect("form");
            assert_eq!(form.template, "Ship $ARGUMENTS");
            assert_eq!(form.description, "About ship");
            assert_eq!(
                page.editor
                    .as_ref()
                    .and_then(|editor| editor.path.as_deref()),
                Some("/config/command/ship.md")
            );
        });
        assert!(
            writes(&fixture).is_empty(),
            "opening a command writes nothing"
        );
    }

    #[gpui_kit::test]
    fn a_failed_list_read_offers_retry_and_never_an_empty_list(cx: &mut TestAppContext) {
        let fixture = fixture();
        fixture.fail_list.store(true, Ordering::Release);
        let (window, workspace) = open(&fixture.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        cx.read(|cx| assert_eq!(page.read(cx).list, Load::Failed));
        act(window, &page, cx, |page, window, cx| {
            page.create_new(window, cx)
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("commands-load-failed-retry").is_some());
            assert!(
                window.try_find("commands-empty").is_none(),
                "a failed read is not an empty list"
            );
            assert!(
                window.try_find("commands-editor").is_none(),
                "no draft without a list"
            );
        })
        .expect("test window");
        fixture.fail_list.store(false, Ordering::Release);
        act(window, &page, cx, |page, _, cx| page.refresh(cx));
        cx.read(|cx| assert_eq!(page.read(cx).commands().len(), 2));
    }

    #[gpui_kit::test]
    fn an_edit_autosaves_only_when_the_form_differs(cx: &mut TestAppContext) {
        let fixture = fixture();
        let (window, workspace) = open(&fixture.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.select("ship", window, cx)
        });
        act(window, &page, cx, |page, window, cx| {
            page.request_save(window, cx)
        });
        assert!(
            writes(&fixture).is_empty(),
            "an unchanged form writes nothing"
        );
        act(window, &page, cx, |page, window, cx| {
            set_template(page, "Ship it now", window, cx);
            page.set_subagent(true, window, cx);
        });
        let sent = writes(&fixture);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].is("PATCH", "/api/config/commands/ship"));
        assert_eq!(sent[0].body["template"], "Ship it now");
        assert_eq!(sent[0].body["subagent"], true);
        assert_eq!(sent[0].body["agent"], Value::Null);
        cx.read(|cx| {
            let page = page.read(cx);
            assert!(!page.has_failed_writes());
            assert_eq!(
                page.editor.as_ref().expect("editor").saved.template,
                "Ship it now",
                "the written form is the new baseline"
            );
        });
        act(window, &page, cx, |page, window, cx| {
            page.request_save(window, cx)
        });
        assert_eq!(
            writes(&fixture).len(),
            1,
            "the saved form is not sent again"
        );
    }

    #[gpui_kit::test]
    fn a_failed_save_keeps_the_edit_and_retry_sends_it(cx: &mut TestAppContext) {
        let fixture = fixture();
        let (window, workspace) = open(&fixture.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.select("ship", window, cx)
        });
        fixture.fail_writes.store(true, Ordering::Release);
        act(window, &page, cx, |page, window, cx| {
            set_template(page, "Kept edit", window, cx);
            page.request_save(window, cx);
        });
        cx.read(|cx| {
            let page = page.read(cx);
            assert!(page.has_failed_writes());
            assert_eq!(page.form(cx).expect("form").template, "Kept edit");
            assert_eq!(
                page.editor.as_ref().expect("editor").saved.template,
                "Ship $ARGUMENTS",
                "a failed write does not move the baseline"
            );
            assert_eq!(page.notice.as_deref(), Some("Failed to update command"));
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("commands-save-failed-retry").is_some());
        })
        .expect("test window");
        fixture.fail_writes.store(false, Ordering::Release);
        act(window, &page, cx, |page, window, cx| {
            page.request_save(window, cx)
        });
        assert_eq!(
            fixture.store.lock().expect("store")[1]["template"],
            "Kept edit"
        );
        cx.read(|cx| assert!(!page.read(cx).has_failed_writes()));
    }

    #[gpui_kit::test]
    fn a_save_that_fails_after_switching_commands_is_still_reported(cx: &mut TestAppContext) {
        let fixture = fixture();
        let (window, workspace) = open(&fixture.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.select("ship", window, cx)
        });
        fixture.fail_writes.store(true, Ordering::Release);
        act(window, &page, cx, |page, window, cx| {
            set_template(page, "Edit left behind", window, cx);
            // Selecting another command commits the open one first.
            page.select("review", window, cx);
        });
        let sent = writes(&fixture);
        assert_eq!(sent.len(), 1, "the left command's text is sent once");
        assert!(sent[0].is("PATCH", "/api/config/commands/ship"));
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.selected.as_deref(), Some("review"));
            assert_eq!(
                page.notice.as_deref(),
                Some("Failed to update command"),
                "the failed write is reported after the switch"
            );
            assert_eq!(page.form(cx).expect("form").template, "Review the diff");
        });
    }

    fn patches(fixture: &Fixture) -> Vec<String> {
        writes(fixture)
            .iter()
            .filter(|write| write.method == "PATCH")
            .map(|write| {
                write.body["template"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }

    /// An edit made while a write is in flight is accepted as a second write
    /// by whatever leaves the editor.
    fn assert_edit_during_a_write_is_kept(switch: bool, cx: &mut TestAppContext) {
        let gate = ResponseGate::new();
        let fixture = gated_fixture(Some(("PATCH", Arc::clone(&gate))));
        let (window, workspace) = open(&fixture.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.select("ship", window, cx)
        });
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                set_template(page, "First", window, cx);
                page.request_save(window, cx);
            });
        })
        .expect("test window");
        gate.entered(cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                set_template(page, "Typed during the write", window, cx);
                if switch {
                    page.select("review", window, cx);
                } else {
                    page.prepare_close(window, cx);
                }
            });
        })
        .expect("test window");
        let settings = settings(&workspace, cx);
        let pending = settings.update(cx, |settings, cx| settings.take_pending_write(cx));
        assert!(pending.is_some(), "closing waits for both accepted writes");
        gate.release();
        cx.foreground_executor().block_test(async move {
            let _ = pending.expect("close receipt").await;
        });
        settle(window, cx);
        assert_eq!(
            patches(&fixture),
            ["First", "Typed during the write"],
            "the later edit is written after the first, in order"
        );
        assert_eq!(
            fixture.store.lock().expect("store")[1]["template"],
            "Typed during the write"
        );
    }

    #[gpui_kit::test]
    fn an_edit_during_a_write_is_kept_on_close(cx: &mut TestAppContext) {
        assert_edit_during_a_write_is_kept(false, cx);
    }

    #[gpui_kit::test]
    fn an_edit_during_a_write_is_kept_on_switch(cx: &mut TestAppContext) {
        assert_edit_during_a_write_is_kept(true, cx);
    }

    #[gpui_kit::test]
    fn an_edit_during_a_write_drains_on_app_quit(cx: &mut TestAppContext) {
        let gate = ResponseGate::new();
        let fixture = gated_fixture(Some(("PATCH", Arc::clone(&gate))));
        let (window, workspace) = open(&fixture.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.select("ship", window, cx)
        });
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                set_template(page, "First", window, cx);
                page.request_save(window, cx);
            });
        })
        .expect("test window");
        gate.entered(cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                set_template(page, "Typed during the write", window, cx);
                page.request_save(window, cx);
            });
        })
        .expect("test window");
        // Quit transfers the I/O of every accepted write.
        let quit = page.update(cx, |page, cx| page.take_quit_write(cx));
        gate.release();
        cx.executor().allow_parking();
        cx.foreground_executor().block_test(async move {
            quit.expect("accepted writes").await;
        });
        assert_eq!(patches(&fixture), ["First", "Typed during the write"]);
    }

    #[gpui_kit::test]
    fn no_save_is_accepted_behind_a_queued_delete(cx: &mut TestAppContext) {
        let gate = ResponseGate::new();
        let fixture = gated_fixture(Some(("DELETE", Arc::clone(&gate))));
        let (window, workspace) = open(&fixture.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.select("ship", window, cx)
        });
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.remove("ship".into(), false, window, cx);
            });
        })
        .expect("test window");
        gate.entered(cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                // The editor is still open while the delete is in flight.
                set_template(page, "Would recreate the file", window, cx);
                page.request_save(window, cx);
                page.prepare_close(window, cx);
                // A second delete of the same command is refused too.
                page.remove("ship".into(), false, window, cx);
            });
        })
        .expect("test window");
        gate.release();
        settle(window, cx);
        let sent = writes(&fixture);
        assert_eq!(sent.len(), 1, "one delete and no save behind it");
        assert!(sent[0].is("DELETE", "/api/config/commands/ship"));
        assert_eq!(
            fixture.store.lock().expect("store").len(),
            1,
            "the deleted command is not recreated"
        );
        cx.read(|cx| assert!(page.read(cx).deleting.is_empty()));
    }

    #[gpui_kit::test]
    fn a_hidden_page_reads_nothing_when_the_owner_changes(cx: &mut TestAppContext) {
        let first = fixture();
        let second = fixture();
        let (window, workspace) = open(&first.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.select("ship", window, cx);
            page.set_shown(false, cx);
        });
        let reads = cx.read(|cx| page.read(cx).read_revision);
        retarget(&workspace, &second.server, cx);
        settle(window, cx);
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.read_revision, reads, "a hidden page starts no read");
            assert!(page.editor.is_none(), "the old server's editor is let go");
            assert_eq!(page.list, Load::Loading);
        });
        act(window, &page, cx, |page, _, cx| page.set_shown(true, cx));
        cx.read(|cx| assert_eq!(page.read(cx).commands().len(), 2));
    }

    #[gpui_kit::test]
    fn skills_are_left_out_of_the_command_list(cx: &mut TestAppContext) {
        let fixture = fixture();
        fixture
            .store
            .lock()
            .expect("store")
            .push(entry("a-skill", "Skill body"));
        let (_, workspace) = open(&fixture.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        cx.read(|cx| {
            let names: Vec<_> = page
                .read(cx)
                .commands()
                .iter()
                .map(|command| command.name.clone())
                .collect();
            assert_eq!(names, ["review", "ship"]);
        });
    }

    #[gpui_kit::test]
    fn an_emptied_template_is_refused_before_any_request(cx: &mut TestAppContext) {
        let fixture = fixture();
        let (window, workspace) = open(&fixture.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.select("ship", window, cx)
        });
        act(window, &page, cx, |page, window, cx| {
            set_template(page, "  ", window, cx);
            page.request_save(window, cx);
        });
        assert!(writes(&fixture).is_empty());
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).notice.as_deref(),
                Some("Command template is required")
            );
        });
    }

    #[gpui_kit::test]
    fn a_draft_is_checked_then_created_and_becomes_the_saved_command(cx: &mut TestAppContext) {
        let fixture = fixture();
        let (window, workspace) = open(&fixture.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.create_new(window, cx)
        });
        act(window, &page, cx, |page, window, cx| {
            let name = page.editor.as_ref().expect("editor").name.clone();
            name.update(cx, |input, cx| input.set_value("ship", window, cx));
            set_template(page, "Body", window, cx);
            // A draft never autosaves.
            page.request_save(window, cx);
            page.create(window, cx);
        });
        assert!(
            writes(&fixture).is_empty(),
            "a taken name is refused locally"
        );
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).notice.as_deref(),
                Some("A command with this name already exists")
            );
        });
        act(window, &page, cx, |page, window, cx| {
            let name = page.editor.as_ref().expect("editor").name.clone();
            name.update(cx, |input, cx| input.set_value("my release", window, cx));
            page.create(window, cx);
        });
        let sent = writes(&fixture);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].is("POST", "/api/config/commands/my-release"));
        assert_eq!(
            sent[0].body,
            json!({"template": "Body", "subagent": false, "scope": "user"})
        );
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.selected.as_deref(), Some("my-release"));
            assert_eq!(
                page.editor
                    .as_ref()
                    .and_then(|editor| editor.saved_name.as_deref()),
                Some("my-release")
            );
            assert_eq!(page.commands().len(), 3, "the list is read again");
        });
    }

    #[gpui_kit::test]
    fn deleting_the_open_command_clears_the_selection(cx: &mut TestAppContext) {
        let fixture = fixture();
        let (window, workspace) = open(&fixture.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.select("ship", window, cx)
        });
        act(window, &page, cx, |page, window, cx| {
            page.remove("ship".into(), false, window, cx)
        });
        assert!(writes(&fixture)[0].is("DELETE", "/api/config/commands/ship"));
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.selected, None);
            assert!(page.editor.is_none());
            assert_eq!(page.commands().len(), 1);
            assert_eq!(
                page.notice.as_deref(),
                Some("Command \"ship\" deleted successfully")
            );
        });
    }

    #[gpui_kit::test]
    fn a_runtime_switch_retires_the_editor_and_its_pending_save(cx: &mut TestAppContext) {
        let first = fixture();
        let second = fixture();
        let (window, workspace) = open(&first.server, Page::Commands, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.select("ship", window, cx)
        });
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                set_template(page, "Old server edit", window, cx);
                page.request_save(window, cx);
            });
        })
        .expect("test window");
        retarget(&workspace, &second.server, cx);
        settle(window, cx);
        cx.read(|cx| {
            let page = page.read(cx);
            assert!(page.editor.is_none(), "the old server's editor is gone");
            assert_eq!(page.selected, None);
            assert!(!page.has_failed_writes());
            assert_eq!(page.commands().len(), 2, "the new server's list is read");
        });
        assert!(
            writes(&second).is_empty(),
            "the retired edit is never written to the new server"
        );
    }
}
