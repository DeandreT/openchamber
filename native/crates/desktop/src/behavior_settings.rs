//! Settings → Behavior (`BehaviorPage.tsx`): the global `AGENTS.md` and the
//! response style. The prompt goes to `AGENTS.md`, guarded by the content last
//! read so a file edited elsewhere is never overwritten; the response style
//! and the prompt's persisted copy go to the settings document. Autosave:
//! toggles and the preset write at once, text writes when it loses focus.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gpui_kit::component::IndexPath;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Theme, WindowExt};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, EntityInputHandler, FocusHandle, InteractiveElement,
    IntoElement, ParentElement, Render, ScrollAnchor, ScrollHandle, SharedString, Styled,
    Subscription, Task, WeakEntity, Window, div, px,
};
use openchamber_native_transport::behavior::{
    AgentsMd, AgentsMdError, BehaviorChanges, BehaviorSettings, RESPONSE_STYLE_PRESETS,
    response_style_instructions,
};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::error::TransportError;

use crate::baseline::CssTextExt;
use crate::locale;
use crate::network::{Network, NetworkTask};
use crate::settings_controls::{
    Load, ServerTarget, SettingsWrites, checkbox_row, field_row, focus_handle, helper, load_failed,
    page_frame, same_target, section_with_info,
};
use crate::settings_navigation::Page;
use crate::theme::Palette;
use crate::workspace::Workspace;

const SYSTEM_PROMPT: usize = 0;
const RESPONSE_STYLE: usize = 1;

/// What is on disk and in the settings document, as last read or written.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Saved {
    prompt: String,
    enabled: bool,
    preset: String,
    custom: String,
}

/// The page's form, compared field by field against `Saved`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Form {
    prompt: String,
    enabled: bool,
    preset: String,
    custom: String,
}

/// The loaded state: the saved baseline and `AGENTS.md` as on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Loaded {
    saved: Saved,
    /// `None` when the file did not exist or could not be read.
    on_disk: Option<String>,
    path: String,
}

struct SubmittedForm {
    revision: u64,
    form: Form,
}

/// One connection's autosave baseline and newest accepted follow-up. The I/O
/// loop owns this state independently of the window and coalesces while busy.
struct Writer {
    loaded: Loaded,
    pending: Option<SubmittedForm>,
    running: bool,
}

struct WriteOutcome {
    loaded: Loaded,
    file_written: bool,
    result: Result<(), SaveFailure>,
}

async fn write_form(client: &ServerClient, form: &Form, mut loaded: Loaded) -> WriteOutcome {
    let prompt_changed = form.prompt != loaded.saved.prompt;
    let settings_changed = form.enabled != loaded.saved.enabled
        || form.preset != loaded.saved.preset
        || form.custom != loaded.saved.custom;
    let mut file_written = false;
    let result = async {
        if !prompt_changed && !settings_changed {
            return Ok(());
        }
        let content = if prompt_changed {
            normalize(&form.prompt)
        } else {
            loaded.saved.prompt.clone()
        };
        if prompt_changed {
            match client
                .save_agents_md(&content, loaded.on_disk.as_deref())
                .await
            {
                Ok(()) => {
                    file_written = true;
                    loaded.on_disk = Some(content.clone());
                }
                Err(AgentsMdError::Conflict) => return Err(SaveFailure::ChangedOnDisk),
                Err(error) => {
                    return Err(error
                        .message()
                        .filter(|message| !message.trim().is_empty())
                        .map_or(SaveFailure::Failed, |message| {
                            SaveFailure::Server(message.to_owned())
                        }));
                }
            }
        }
        let changes = BehaviorChanges {
            global_behavior_prompt: prompt_changed.then(|| content.clone()),
            response_style_enabled: form.enabled,
            response_style_preset: form.preset.clone(),
            response_style_custom_instructions: form.custom.clone(),
        };
        client
            .save_behavior_settings(&changes)
            .await
            .map_err(|_| SaveFailure::Failed)?;
        loaded.saved = Saved {
            prompt: content,
            enabled: form.enabled,
            preset: form.preset.clone(),
            custom: form.custom.clone(),
        };
        Ok(())
    }
    .await;
    WriteOutcome {
        loaded,
        file_written,
        result,
    }
}

/// The prompt shown: an existing file wins even when empty; the persisted
/// copy fills in for a missing file or a failed read (`resolveBehaviorPrompt`).
fn resolve_prompt(file: Option<&AgentsMd>, settings: &BehaviorSettings) -> String {
    match file.and_then(|file| file.content.as_ref()) {
        Some(content) => content.clone(),
        None => settings.global_behavior_prompt.clone().unwrap_or_default(),
    }
}

/// A non-empty prompt ends with one newline (`normalizeAgentsMdContent`).
fn normalize(content: &str) -> String {
    if content.is_empty() || content.ends_with('\n') {
        content.to_owned()
    } else {
        format!("{content}\n")
    }
}

/// Why a save did not land, as the autosave toast names it.
#[derive(Debug, PartialEq, Eq)]
enum SaveFailure {
    ChangedOnDisk,
    Server(String),
    Failed,
}

#[derive(Clone, PartialEq, Eq)]
struct PresetOption {
    value: String,
    label: SharedString,
}

impl SearchableListItem for PresetOption {
    type Value = String;
    fn title(&self) -> SharedString {
        self.label.clone()
    }
    fn value(&self) -> &String {
        &self.value
    }
}

fn preset_label(preset: &str) -> &'static str {
    match preset {
        "detailed" => "settings.behavior.page.responseStyle.option.detailed",
        "mentor" => "settings.behavior.page.responseStyle.option.mentor",
        "pushback" => "settings.behavior.page.responseStyle.option.pushback",
        "noFiller" => "settings.behavior.page.responseStyle.option.noFiller",
        "matchEnergy" => "settings.behavior.page.responseStyle.option.matchEnergy",
        "warmPeer" => "settings.behavior.page.responseStyle.option.warmPeer",
        "custom" => "settings.behavior.page.responseStyle.option.custom",
        _ => "settings.behavior.page.responseStyle.option.concise",
    }
}

pub(crate) struct BehaviorPage {
    owner: WeakEntity<Workspace>,
    network: Option<Network>,
    target: Option<ServerTarget>,
    load: Load<Loaded>,
    enabled: bool,
    preset: String,
    custom: String,
    prompt: Entity<TextareaState>,
    preview: Entity<TextareaState>,
    preset_picker: Entity<SelectState<SearchableVec<PresetOption>>>,
    writes: SettingsWrites,
    writer: Option<Arc<Mutex<Writer>>>,
    form_revision: u64,
    applied_write: u64,
    last_submitted: Option<Form>,
    /// The last save's failure, shown as a toast and kept for retry.
    failure: Option<SharedString>,
    /// Server values the text fields take on the next render, which has
    /// the window an editor update needs.
    pending_values: Option<Saved>,
    pending_prompt: Option<String>,
    read_revision: u64,
    read: Option<Task<()>>,
    scroll: ScrollHandle,
    anchors: [ScrollAnchor; 2],
    focus: HashMap<SharedString, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl BehaviorPage {
    pub(crate) fn new(
        owner: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let strings = locale::strings(cx).clone();
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(12, 30)
                .placeholder(strings.get("settings.behavior.page.field.systemPromptPlaceholder"))
        });
        let preview = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(5, 12)
                .placeholder(strings.get("settings.behavior.page.responseStyle.customPlaceholder"))
        });
        let options = Self::preset_options(&strings);
        let preset_picker = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(options),
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        let mut subscriptions = vec![
            cx.subscribe_in(&prompt, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.read_revision += 1;
                    this.pending_prompt = None;
                }
                if matches!(event, InputEvent::Blur) {
                    this.request_save(window, cx);
                }
            }),
            cx.subscribe_in(
                &preview,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::Change if this.preset == "custom" => {
                        this.read_revision += 1;
                        this.custom = input.read(cx).value().to_string();
                    }
                    InputEvent::Blur => this.request_save(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(&preset_picker, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(preset)) = event {
                    this.set_preset(preset.clone(), window, cx);
                }
            }),
            // AGENTS.md is often edited elsewhere: coming back re-reads it.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.reread_file(cx);
                }
            }),
        ];
        if let Some(workspace) = owner.upgrade() {
            subscriptions.push(cx.observe(&workspace, |this, _, cx| {
                let target = ServerTarget::capture(&this.owner, this.network.as_ref(), cx);
                if !same_target(this.target.as_ref(), target.as_ref()) {
                    this.refresh(cx);
                }
            }));
        }
        let scroll = ScrollHandle::new();
        let weak = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = weak.update(cx, |this, cx| this.refresh(cx));
        });
        Self {
            owner,
            network: Network::new().ok(),
            target: None,
            load: Load::Loading,
            enabled: false,
            preset: "concise".into(),
            custom: String::new(),
            prompt,
            preview,
            preset_picker,
            writes: SettingsWrites::default(),
            writer: None,
            form_revision: 0,
            applied_write: 0,
            last_submitted: None,
            failure: None,
            pending_values: None,
            pending_prompt: None,
            read_revision: 0,
            read: None,
            anchors: std::array::from_fn(|_| ScrollAnchor::for_handle(scroll.clone())),
            scroll,
            focus: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    fn preset_options(strings: &locale::Strings) -> Vec<PresetOption> {
        RESPONSE_STYLE_PRESETS
            .iter()
            .copied()
            .chain(["custom"])
            .map(|preset| PresetOption {
                value: preset.to_owned(),
                label: strings.get(preset_label(preset)),
            })
            .collect()
    }

    /// Search targets are the canonical registry IDs.
    pub(crate) fn reveal(&mut self, item: &str, window: &mut Window, cx: &mut Context<Self>) {
        let index = match item {
            "behavior.system-prompt" => SYSTEM_PROMPT,
            "behavior.response-style" => RESPONSE_STYLE,
            _ => return,
        };
        cx.on_next_frame(window, move |this, window, cx| {
            this.anchors[index].scroll_to(window, cx);
            if index == SYSTEM_PROMPT {
                this.prompt.update(cx, |input, cx| input.focus(window, cx));
            } else if let Some(focus) = this.focus.get("behavior-response-style-enabled") {
                window.focus(focus, cx);
            }
        });
    }

    /// Reads both sources against the current server.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let target = ServerTarget::capture(&self.owner, self.network.as_ref(), cx);
        if !same_target(self.target.as_ref(), target.as_ref()) {
            // Another server's form and baseline never carry over.
            self.load = Load::Loading;
            self.failure = None;
            self.writer = None;
            self.last_submitted = None;
            self.pending_values = None;
            self.pending_prompt = None;
        } else if self.load.ready().is_some_and(|loaded| {
            let form = self.form(cx);
            form.prompt != loaded.saved.prompt
                || form.enabled != loaded.saved.enabled
                || form.preset != loaded.saved.preset
                || form.custom != loaded.saved.custom
        }) || self.writes.is_pending()
        {
            // Revisiting Settings cannot erase an unconfirmed or failed edit.
            return;
        }
        self.read_revision += 1;
        self.target = target.clone();
        let Some(target) = target else {
            self.read = None;
            cx.notify();
            return;
        };
        if self.load == Load::Failed {
            self.load = Load::Loading;
        }
        let revision = self.read_revision;
        let client = target.client.clone();
        let reading = NetworkTask::spawn_on(&target.network, async move {
            let (settings, file) = tokio::join!(client.behavior_settings(), client.agents_md());
            settings.map(|settings| (settings, file.ok()))
        });
        self.read = Some(cx.spawn(async move |this, cx| {
            let result = reading.await;
            let _ = this.update(cx, |this, cx| {
                if this.read_revision != revision {
                    return;
                }
                this.read = None;
                this.finish_read(result, cx);
            });
        }));
        cx.notify();
    }

    fn finish_read(
        &mut self,
        result: Option<Result<(BehaviorSettings, Option<AgentsMd>), TransportError>>,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok((settings, file))) = result else {
            // A form already loaded keeps its baseline; a first read fails visibly.
            if self.load.ready().is_none() {
                self.load = Load::Failed;
            }
            cx.notify();
            return;
        };
        let loaded = Loaded {
            saved: Saved {
                prompt: resolve_prompt(file.as_ref(), &settings),
                enabled: settings.response_style_enabled,
                preset: settings.response_style_preset.clone(),
                custom: settings.response_style_custom_instructions.clone(),
            },
            on_disk: file.as_ref().and_then(|file| file.content.clone()),
            path: file
                .and_then(|file| file.path)
                .unwrap_or_else(|| "AGENTS.md".to_owned()),
        };
        let saved = loaded.saved.clone();
        self.writer = Some(Arc::new(Mutex::new(Writer {
            loaded: loaded.clone(),
            pending: None,
            running: false,
        })));
        self.load = Load::Ready(loaded);
        self.enabled = saved.enabled;
        self.preset = saved.preset.clone();
        self.custom = saved.custom.clone();
        self.pending_values = Some(saved);
        cx.notify();
    }

    /// Coming back re-reads `AGENTS.md`; the editor follows only while it
    /// holds no edit of its own.
    fn reread_file(&mut self, cx: &mut Context<Self>) {
        if self.writes.is_pending() {
            return;
        }
        let (Some(target), Some(_)) = (self.target.clone(), self.load.ready()) else {
            return;
        };
        self.read_revision += 1;
        let revision = self.read_revision;
        let client = target.client.clone();
        let reading =
            NetworkTask::spawn_on(&target.network, async move { client.agents_md().await });
        cx.spawn(async move |this, cx| {
            let Some(Ok(file)) = reading.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.read_revision != revision
                    || !same_target(this.target.as_ref(), Some(&target))
                    || !same_target(
                        ServerTarget::capture(&this.owner, this.network.as_ref(), cx).as_ref(),
                        Some(&target),
                    )
                {
                    return;
                }
                let Some(content) = file.content else {
                    return;
                };
                let current = this.prompt.read(cx).value().to_string();
                let Some(loaded) = this.load.ready_mut() else {
                    return;
                };
                if loaded.on_disk.as_deref() == Some(content.as_str())
                    || current != loaded.saved.prompt
                {
                    return;
                }
                loaded.on_disk = Some(content.clone());
                loaded.saved.prompt = content.clone();
                if let Some(writer) = &this.writer {
                    writer.lock().expect("Behavior writer").loaded = loaded.clone();
                }
                this.pending_prompt = Some(content);
                cx.notify();
            });
        })
        .detach();
    }

    fn form(&self, cx: &Context<Self>) -> Form {
        Form {
            prompt: self.prompt.read(cx).value().to_string(),
            enabled: self.enabled,
            preset: self.preset.clone(),
            custom: self.custom.clone(),
        }
    }

    fn editable(&self) -> bool {
        self.load.ready().is_some() && self.target.is_some() && !self.writes.closing()
    }

    fn set_enabled(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        self.enabled = enabled;
        self.request_save(window, cx);
    }

    fn set_preset(&mut self, preset: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable() || !self.enabled || preset == self.preset {
            return;
        }
        self.preset = preset;
        self.show_preview(window, cx);
        self.request_save(window, cx);
    }

    /// The preview shows the preset's instructions, or the custom text to edit.
    fn show_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = if self.preset == "custom" {
            self.custom.clone()
        } else {
            response_style_instructions(&self.preset)
                .unwrap_or_default()
                .to_owned()
        };
        self.preview
            .update(cx, |input, cx| input.set_value(text, window, cx));
    }

    /// Starts a save, or queues one follow-up behind the running one.
    fn request_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable() || self.pending_values.is_some() || self.pending_prompt.is_some() {
            return;
        }
        let (Some(target), Some(writer)) = (self.target.clone(), self.writer.clone()) else {
            return;
        };
        let form = self.form(cx);
        {
            let mut writer = writer.lock().expect("Behavior writer");
            let saved = &writer.loaded.saved;
            if !writer.running
                && form.prompt == saved.prompt
                && form.enabled == saved.enabled
                && form.preset == saved.preset
                && form.custom == saved.custom
            {
                return;
            }
            self.form_revision += 1;
            self.last_submitted = Some(form.clone());
            writer.pending = Some(SubmittedForm {
                revision: self.form_revision,
                form,
            });
            if writer.running {
                return;
            }
            writer.running = true;
        }
        let client = target.client.clone();
        let (completed, mut outcomes) = tokio::sync::mpsc::unbounded_channel();
        let (id, receipt) = self.writes.submit(&target, async move {
            loop {
                let next = {
                    let mut writer = writer.lock().expect("Behavior writer");
                    match writer.pending.take() {
                        Some(form) => Some((form, writer.loaded.clone())),
                        None => {
                            writer.running = false;
                            None
                        }
                    }
                };
                let Some((submitted, loaded)) = next else {
                    break;
                };
                let outcome = write_form(&client, &submitted.form, loaded).await;
                {
                    let mut writer = writer.lock().expect("Behavior writer");
                    writer.loaded = outcome.loaded.clone();
                    // A queued toggle carries the same prompt until normalization.
                    if outcome.file_written
                        && let Some(queued) = &mut writer.pending
                        && queued.form.prompt == submitted.form.prompt
                        && let Some(content) = &outcome.loaded.on_disk
                    {
                        queued.form.prompt = content.clone();
                    }
                }
                let _ = completed.send((submitted, outcome));
            }
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            while let Some((submitted, outcome)) = outcomes.recv().await {
                let _ = this.update_in(cx, |this, window, cx| {
                    if !same_target(this.target.as_ref(), Some(&target))
                        || !same_target(
                            ServerTarget::capture(&this.owner, this.network.as_ref(), cx).as_ref(),
                            Some(&target),
                        )
                        || submitted.revision < this.applied_write
                    {
                        return;
                    }
                    this.applied_write = submitted.revision;
                    if outcome.file_written
                        && this.prompt.read(cx).value().as_str() == submitted.form.prompt
                        && let Some(content) = &outcome.loaded.on_disk
                    {
                        this.prompt
                            .update(cx, |input, cx| input.set_value(content.clone(), window, cx));
                        if let Some(last) = &mut this.last_submitted
                            && last.prompt == submitted.form.prompt
                        {
                            last.prompt = content.clone();
                        }
                    }
                    this.load = Load::Ready(outcome.loaded);
                    match outcome.result {
                        Ok(()) => this.failure = None,
                        Err(failure) => this.fail(failure, window, cx),
                    }
                    cx.notify();
                });
            }
            let result = receipt.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if result.is_err() && same_target(this.target.as_ref(), Some(&target)) {
                    this.fail(SaveFailure::Failed, window, cx);
                }
                this.writes.finish(id);
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    pub(crate) fn prepare_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let composing = self.prompt.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        }) || self.preview.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        });
        if !composing
            && (self.failure.is_none() || self.last_submitted.as_ref() != Some(&self.form(cx)))
        {
            self.request_save(window, cx);
        }
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
        self.failure.is_some()
    }

    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.writes.cancel_close();
        cx.notify();
    }

    pub(crate) fn take_quit_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.writes.take_quit_write(self.network.take(), cx)
    }

    fn fail(&mut self, failure: SaveFailure, window: &mut Window, cx: &mut Context<Self>) {
        let strings = locale::strings(cx);
        let reason = match failure {
            SaveFailure::ChangedOnDisk => {
                strings.get("settings.behavior.page.toast.agentsMdChangedOnDisk")
            }
            SaveFailure::Server(message) => message.into(),
            SaveFailure::Failed => strings.get("settings.behavior.page.toast.saveFailed"),
        };
        let text = strings.format(
            "settings.common.status.saveFailedReason",
            &[("reason", reason.as_ref())],
        );
        window.push_notification(Notification::error(text.clone()), cx);
        self.failure = Some(text);
    }
}

impl Render for BehaviorPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(saved) = self.pending_values.take() {
            self.prompt
                .update(cx, |input, cx| input.set_value(saved.prompt, window, cx));
            self.show_preview(window, cx);
        }
        if let Some(prompt) = self.pending_prompt.take() {
            self.prompt
                .update(cx, |input, cx| input.set_value(prompt, window, cx));
        }
        let strings = locale::strings(cx).clone();
        if self.preset_picker.read(cx).selected_value() != Some(&self.preset) {
            let preset = self.preset.clone();
            self.preset_picker.update(cx, |picker, cx| {
                picker.set_items(
                    SearchableVec::new(Self::preset_options(&strings)),
                    window,
                    cx,
                );
                picker.set_selected_value(&preset, window, cx);
            });
        }
        let editable = self.editable();
        let custom = self.preset == "custom";
        let preview_editable = editable && self.enabled && custom;
        let palette = cx.global::<Palette>().clone();
        let mut frame = page_frame("behavior-settings-page", Page::Behavior, &self.scroll, cx);
        let body: Vec<AnyElement> = match &self.load {
            Load::Failed => vec![load_failed(
                "behavior-load-failed",
                strings.get("settings.openchamber.desktopNetwork.error.loadFailed"),
                strings.get("desktopHostSwitcher.actions.retry"),
                cx,
                |this, cx| this.refresh(cx),
            )],
            _ if self.target.is_none() => vec![helper(strings.get("common.unavailable"), cx)],
            load => {
                let path = load
                    .ready()
                    .map_or("AGENTS.md", |loaded| loaded.path.as_str())
                    .to_owned();
                let warning: SharedString = format!(
                    "{}\n{}",
                    strings.get("settings.behavior.page.warning.title"),
                    strings.format(
                        "settings.behavior.page.warning.description",
                        &[("path", &path)]
                    )
                )
                .into();
                let prompt = div()
                    .css_text(0.8125, 1.25, cx)
                    .id("behavior-system-prompt")
                    .w_full()
                    .font_family(Theme::global(cx).mono_font_family.clone())
                    .child(
                        Textarea::new(&self.prompt)
                            .accessibility_id("behavior-system-prompt-input")
                            .aria_label(strings.get("settings.behavior.page.section.systemPrompt"))
                            .disabled(!editable),
                    )
                    .into_any_element();
                let enabled_id: SharedString = "behavior-response-style-enabled".into();
                let focus = focus_handle(&mut self.focus, &enabled_id, cx);
                let checkbox = checkbox_row(
                    enabled_id,
                    strings.get("settings.behavior.page.responseStyle.enable"),
                    strings.get("settings.behavior.page.responseStyle.enableAria"),
                    self.enabled,
                    editable,
                    &focus,
                    None,
                    cx,
                    |this: &mut Self, value, window, cx| this.set_enabled(value, window, cx),
                );
                let picker = div()
                    .key_context("SettingsPicker")
                    .w(px(240.0))
                    .max_w_full()
                    .h(gpui_kit::rems(2.0))
                    .child(
                        Select::new(&self.preset_picker)
                            .id("behavior-response-style-preset")
                            .accessibility_label(
                                strings.get("settings.behavior.page.responseStyle.preset"),
                            )
                            .disabled(!editable || !self.enabled)
                            .w_full()
                            .h(gpui_kit::rems(2.0)),
                    )
                    .into_any_element();
                let preset = field_row(
                    "behavior.response-style-preset",
                    strings.get("settings.behavior.page.responseStyle.preset"),
                    None,
                    picker,
                    None,
                    cx,
                );
                let preview = div()
                    .css_text(0.8125, 1.25, cx)
                    .id("behavior-response-style-preview")
                    .w_full()
                    .font_family(Theme::global(cx).mono_font_family.clone())
                    .child(
                        Textarea::new(&self.preview)
                            .accessibility_id("behavior-response-style-text")
                            .aria_label(strings.get("settings.behavior.page.section.responseStyle"))
                            .disabled(!preview_editable),
                    )
                    .into_any_element();
                vec![
                    section_with_info(
                        "settings.behavior.page.section.systemPrompt",
                        Some(warning),
                        true,
                        vec![prompt],
                        Some(self.anchors[SYSTEM_PROMPT].clone()),
                        cx,
                    ),
                    section_with_info(
                        "settings.behavior.page.section.responseStyle",
                        Some(strings.get("settings.behavior.page.responseStyle.tooltip")),
                        false,
                        vec![checkbox, preset, preview],
                        Some(self.anchors[RESPONSE_STYLE].clone()),
                        cx,
                    ),
                ]
            }
        };
        frame = frame.children(body);
        frame.text_color(palette.foreground)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui_kit::TestAppContext;
    use gpui_kit::test::TestWindowExt;
    use serde_json::{Value, json};

    use super::*;
    use crate::settings_test_server::{Server, open, settings, settle};

    /// A server holding an `AGENTS.md` and a settings document.
    fn server(
        file: Option<&str>,
        conflict: bool,
        settings_ok: bool,
    ) -> (Server, Arc<Mutex<Value>>) {
        let document = Arc::new(Mutex::new(json!({
            "responseStyleEnabled": false,
            "responseStylePreset": "concise",
        })));
        let disk = Arc::new(Mutex::new(file.map(str::to_owned)));
        let held = Arc::clone(&document);
        let server = Server::new(move |request| {
            if request.is("GET", "/api/behavior/agents-md") {
                let disk = disk.lock().expect("disk");
                return (
                    200,
                    json!({
                        "content": disk.clone().unwrap_or_default(),
                        "exists": disk.is_some(),
                        "path": "/home/me/.config/opencode/AGENTS.md",
                    }),
                );
            }
            if request.is("PUT", "/api/behavior/agents-md") {
                let mut disk = disk.lock().expect("disk");
                if conflict || request.body["expectedContent"].as_str() != disk.as_deref() {
                    return (
                        409,
                        json!({"error": "changed", "code": "AGENTS_MD_CONFLICT"}),
                    );
                }
                *disk = request.body["content"].as_str().map(str::to_owned);
                return (200, json!({"success": true}));
            }
            if request.is("GET", "/api/config/settings") {
                if !settings_ok {
                    return (500, json!({"error": "fixture read failure"}));
                }
                return (200, held.lock().expect("settings").clone());
            }
            if request.is("PUT", "/api/config/settings") {
                let mut document = held.lock().expect("settings");
                for (key, value) in request.body.as_object().expect("settings object") {
                    document[key] = value.clone();
                }
                return (200, document.clone());
            }
            (404, json!({"error": "not in this fixture"}))
        });
        (server, document)
    }

    fn page(workspace: &Entity<Workspace>, cx: &TestAppContext) -> Entity<BehaviorPage> {
        let settings = settings(workspace, cx);
        cx.read(|cx| settings.read(cx).behavior.clone())
    }

    fn held_writer() -> (
        Server,
        Arc<crate::settings_controls::tests::ResponseGate>,
        Arc<Mutex<Value>>,
    ) {
        use crate::settings_controls::tests::ResponseGate;
        use std::sync::atomic::{AtomicBool, Ordering};
        let gate = ResponseGate::new();
        let held = Arc::clone(&gate);
        let first = AtomicBool::new(true);
        let document = Arc::new(Mutex::new(json!({})));
        let saved = Arc::clone(&document);
        let file = Mutex::new("Rules".to_owned());
        let server = Server::new(move |request| {
            if request.is("GET", "/api/behavior/agents-md") {
                return (
                    200,
                    json!({"content": file.lock().expect("file").clone(), "exists": true}),
                );
            }
            if request.is("PUT", "/api/behavior/agents-md") {
                if first.swap(false, Ordering::SeqCst) {
                    held.hold();
                }
                let mut file = file.lock().expect("file");
                assert_eq!(
                    request.body["expectedContent"].as_str(),
                    Some(file.as_str()),
                    "each queued save uses the previous write's conflict baseline"
                );
                *file = request.body["content"].as_str().expect("prompt").to_owned();
                return (200, json!({"success": true}));
            }
            if request.is("PUT", "/api/config/settings") {
                for (key, value) in request.body.as_object().expect("settings") {
                    saved.lock().expect("settings")[key] = value.clone();
                }
            }
            (200, saved.lock().expect("settings").clone())
        });
        (server, gate, document)
    }

    fn queue_prompt(
        window: gpui_kit::AnyWindowHandle,
        page: &Entity<BehaviorPage>,
        value: &str,
        cx: &mut TestAppContext,
    ) {
        let value = value.to_owned();
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.prompt
                    .update(cx, |input, cx| input.set_value(value, window, cx));
                page.request_save(window, cx);
            });
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn close_waits_for_the_latest_coalesced_behavior_form(cx: &mut TestAppContext) {
        let (server, gate, document) = held_writer();
        let (window, workspace) = open(&server, Page::Behavior, cx);
        let page = page(&workspace, cx);
        queue_prompt(window, &page, "First", cx);
        gate.entered(cx);
        queue_prompt(window, &page, "Intermediate", cx);
        queue_prompt(window, &page, "Latest", cx);
        let settings = settings(&workspace, cx);
        let pending = settings
            .update(cx, |settings, cx| settings.take_pending_write(cx))
            .expect("close waits");
        gate.release();
        cx.foreground_executor().block_test(async move {
            pending.await;
        });
        settle(window, cx);
        assert_eq!(
            document.lock().expect("settings")["globalBehaviorPrompt"],
            "Latest\n"
        );
        let writes: Vec<_> = server
            .writes()
            .into_iter()
            .filter(|request| request.is("PUT", "/api/behavior/agents-md"))
            .collect();
        assert_eq!(
            writes.len(),
            2,
            "the running form and newest follow-up are saved"
        );
        assert!(!cx.read(|cx| settings.read(cx).has_failed_writes(cx)));
        cx.read(|cx| assert_eq!(page.read(cx).prompt.read(cx).value().as_str(), "Latest\n"));
    }

    #[gpui_kit::test]
    fn app_quit_drains_behavior_forms_without_foreground_completions(cx: &mut TestAppContext) {
        let (server, gate, document) = held_writer();
        let (window, workspace) = open(&server, Page::Behavior, cx);
        let page = page(&workspace, cx);
        queue_prompt(window, &page, "First", cx);
        gate.entered(cx);
        queue_prompt(window, &page, "Latest", cx);
        let settings = settings(&workspace, cx);
        let _normal_close = settings.update(cx, |settings, cx| settings.take_pending_write(cx));
        cx.update(|cx| {
            cx.on_app_quit(move |cx| {
                let pending = settings.update(cx, |settings, cx| settings.take_quit_write(cx));
                gate.release();
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
        assert_eq!(
            document.lock().expect("settings")["globalBehaviorPrompt"],
            "Latest\n",
            "quit returns only after the accepted form is saved"
        );
        assert_eq!(
            server
                .writes()
                .iter()
                .filter(|request| request.is("PUT", "/api/behavior/agents-md"))
                .count(),
            2
        );
    }

    #[gpui_kit::test]
    fn failed_behavior_edits_survive_close_and_revisit_without_automatic_retry(
        cx: &mut TestAppContext,
    ) {
        let (server, _) = server(Some("Rules"), true, true);
        let (window, workspace) = open(&server, Page::Behavior, cx);
        let page = page(&workspace, cx);
        edit_prompt(window, &page, "Retained", cx);
        let settings = settings(&workspace, cx);
        cx.update_window(window, |_, window, cx| {
            settings.update(cx, |settings, cx| settings.prepare_close(window, cx));
        })
        .expect("test window");
        assert!(
            settings
                .update(cx, |settings, cx| settings.take_pending_write(cx))
                .is_none()
        );
        assert!(
            cx.read(|cx| settings.read(cx).has_failed_writes(cx)),
            "normal close must keep the failed form open"
        );
        settings.update(cx, |settings, cx| settings.cancel_close(cx));
        page.update(cx, |page, cx| page.refresh(cx));
        settle(window, cx);
        assert_eq!(
            server.writes().len(),
            1,
            "close and revisit do not replay a failed edit"
        );
        cx.read(|cx| assert_eq!(page.read(cx).prompt.read(cx).value().as_str(), "Retained"));
    }

    fn edit_prompt(
        window: gpui_kit::AnyWindowHandle,
        page: &Entity<BehaviorPage>,
        text: &str,
        cx: &mut TestAppContext,
    ) {
        let text = text.to_owned();
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.prompt
                    .update(cx, |input, cx| input.set_value(text, window, cx));
                // The editor losing focus is the autosave trigger.
                page.request_save(window, cx);
            });
        })
        .expect("test window");
        settle(window, cx);
    }

    #[gpui_kit::test]
    fn review_regression_a_focus_read_cannot_replace_another_servers_prompt(
        cx: &mut TestAppContext,
    ) {
        use crate::settings_controls::tests::{ResponseGate, retarget};
        use std::sync::atomic::{AtomicBool, Ordering};

        let gate = ResponseGate::new();
        let held = Arc::clone(&gate);
        let armed = Arc::new(AtomicBool::new(false));
        let reading_armed = Arc::clone(&armed);
        let first = Server::new(move |request| {
            if request.is("GET", "/api/behavior/agents-md") {
                if reading_armed.load(Ordering::SeqCst) {
                    held.hold();
                }
                return (200, json!({"exists": true, "content": "A rules"}));
            }
            (200, json!({}))
        });
        let (second, _) = server(Some("B rules"), false, true);
        let (window, workspace) = open(&first, Page::Behavior, cx);
        let page = page(&workspace, cx);
        armed.store(true, Ordering::SeqCst);
        page.update(cx, |page, cx| page.reread_file(cx));
        gate.entered(cx);
        retarget(&workspace, &second, cx);
        settle(window, cx);
        gate.release();
        settle(window, cx);
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.prompt.read(cx).value().as_str(), "B rules");
            assert_eq!(
                page.load.ready().expect("B baseline").on_disk.as_deref(),
                Some("B rules")
            );
        });
    }

    #[gpui_kit::test]
    fn review_regression_close_captures_unblurred_behavior_text(cx: &mut TestAppContext) {
        let (server, _) = server(Some("Rules"), false, true);
        let (window, workspace) = open(&server, Page::Behavior, cx);
        let page = page(&workspace, cx);
        let settings = settings(&workspace, cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.prompt
                    .update(cx, |input, cx| input.set_value("Unblurred", window, cx));
            });
            settings.update(cx, |settings, cx| settings.prepare_close(window, cx));
        })
        .expect("test window");
        settle(window, cx);
        assert!(
            server.writes().iter().any(|write| {
                write.is("PUT", "/api/behavior/agents-md") && write.body["content"] == "Unblurred\n"
            }),
            "normal close accepts the retained editor text before draining"
        );
    }

    #[gpui_kit::test]
    fn an_existing_file_wins_and_a_prompt_edit_writes_the_file_then_the_settings(
        cx: &mut TestAppContext,
    ) {
        let (server, document) = server(Some("Old rules"), false, true);
        document.lock().expect("settings")["globalBehaviorPrompt"] = json!("stale copy");
        let (window, workspace) = open(&server, Page::Behavior, cx);
        let page = page(&workspace, cx);
        cx.read(|cx| {
            assert_eq!(page.read(cx).prompt.read(cx).value().as_str(), "Old rules");
        });

        edit_prompt(window, &page, "New rules", cx);
        let writes = server.writes();
        assert_eq!(writes.len(), 2, "one file write and one settings write");
        assert!(writes[0].is("PUT", "/api/behavior/agents-md"));
        assert_eq!(
            writes[0].body,
            json!({"content": "New rules\n", "expectedContent": "Old rules"}),
            "the write is normalized and guarded by the content read"
        );
        assert!(writes[1].is("PUT", "/api/config/settings"));
        assert_eq!(writes[1].body["globalBehaviorPrompt"], "New rules\n");
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(
                page.prompt.read(cx).value().as_str(),
                "New rules\n",
                "the submitted draft takes its normalized form"
            );
            assert_eq!(page.failure, None, "success is silent");
        });

        edit_prompt(window, &page, "New rules\n", cx);
        assert_eq!(server.writes().len(), 2, "an unchanged form writes nothing");
    }

    #[gpui_kit::test]
    fn a_file_changed_elsewhere_is_never_overwritten(cx: &mut TestAppContext) {
        let (server, _) = server(Some("Old rules"), true, true);
        let (window, workspace) = open(&server, Page::Behavior, cx);
        let page = page(&workspace, cx);
        edit_prompt(window, &page, "Mine", cx);
        assert_eq!(
            server.writes().len(),
            1,
            "the settings copy is not written after a refused file write"
        );
        cx.read(|cx| {
            let failure = page.read(cx).failure.clone().expect("failure");
            assert!(failure.contains("AGENTS.md was changed outside OpenChamber"));
            assert_eq!(
                page.read(cx).prompt.read(cx).value().as_str(),
                "Mine",
                "the edit stays for the user to copy"
            );
        });
    }

    #[gpui_kit::test]
    fn a_missing_file_uses_the_persisted_copy(cx: &mut TestAppContext) {
        let (server, document) = server(None, false, true);
        document.lock().expect("settings")["globalBehaviorPrompt"] = json!("Persisted");
        let (window, workspace) = open(&server, Page::Behavior, cx);
        let page = page(&workspace, cx);
        cx.read(|cx| {
            assert_eq!(page.read(cx).prompt.read(cx).value().as_str(), "Persisted");
        });
        edit_prompt(window, &page, "Created", cx);
        assert_eq!(
            server.writes()[0].body["expectedContent"],
            Value::Null,
            "a missing file is expected to stay missing"
        );
    }

    #[gpui_kit::test]
    fn a_failed_read_shows_retry_instead_of_an_empty_editable_form(cx: &mut TestAppContext) {
        let (server, _) = server(Some("Rules"), false, false);
        let (window, workspace) = open(&server, Page::Behavior, cx);
        let page = page(&workspace, cx);
        cx.read(|cx| {
            assert_eq!(page.read(cx).load, Load::Failed);
            assert!(!page.read(cx).editable());
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("behavior-load-failed-retry").is_some());
            assert!(window.try_find("behavior-system-prompt").is_none());
        })
        .expect("test window");
        edit_prompt(window, &page, "Typed anyway", cx);
        assert!(server.writes().is_empty(), "nothing saves before a read");
    }

    #[gpui_kit::test]
    fn the_response_style_toggle_writes_only_settings(cx: &mut TestAppContext) {
        let (server, document) = server(Some("Rules"), false, true);
        let (window, _workspace) = open(&server, Page::Behavior, cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("behavior-response-style-enabled", cx);
        })
        .expect("test window");
        settle(window, cx);
        let writes = server.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(
            writes[0].body,
            json!({
                "responseStyleEnabled": true,
                "responseStylePreset": "concise",
                "responseStyleCustomInstructions": "",
            })
        );
        assert_eq!(
            document.lock().expect("settings")["responseStyleEnabled"],
            true
        );
    }
}
