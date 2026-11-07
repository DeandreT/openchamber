//! Settings → Behavior (`BehaviorPage.tsx`): the global `AGENTS.md` and the
//! response style. The prompt goes to `AGENTS.md`, guarded by the content last
//! read so a file edited elsewhere is never overwritten; the response style
//! and the prompt's persisted copy go to the settings document. Autosave:
//! toggles and the preset write at once, text writes when it loses focus.

use std::collections::HashMap;

use gpui_kit::component::IndexPath;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Theme, WindowExt};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollAnchor, ScrollHandle, SharedString, Styled, Subscription, Task,
    WeakEntity, Window, div, px,
};
use openchamber_native_transport::behavior::{
    AgentsMd, AgentsMdError, BehaviorChanges, BehaviorSettings, RESPONSE_STYLE_PRESETS,
    response_style_instructions,
};
use openchamber_native_transport::error::TransportError;

use crate::baseline::CssTextExt;
use crate::locale;
use crate::network::{Network, NetworkTask};
use crate::settings_controls::{
    Load, SerialSave, ServerTarget, checkbox_row, field_row, focus_handle, helper, load_failed,
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
    save: SerialSave,
    /// The last save's failure, shown as a toast and kept for retry.
    failure: Option<SharedString>,
    /// Server values the text fields take on the next render, which has
    /// the window an editor update needs.
    pending_values: Option<Saved>,
    pending_prompt: Option<String>,
    read_revision: u64,
    read: Option<Task<()>>,
    write: Option<Task<()>>,
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
                if matches!(event, InputEvent::Blur) {
                    this.request_save(window, cx);
                }
            }),
            cx.subscribe_in(&preview, window, |this, input, event, window, cx| {
                match event {
                    InputEvent::Change if this.preset == "custom" => {
                        this.custom = input.read(cx).value().to_string();
                    }
                    InputEvent::Blur => this.request_save(window, cx),
                    _ => {}
                }
            }),
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
            save: SerialSave::default(),
            failure: None,
            pending_values: None,
            pending_prompt: None,
            read_revision: 0,
            read: None,
            write: None,
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
            self.write = None;
            self.save = SerialSave::default();
        }
        self.target = target.clone();
        let Some(target) = target else {
            self.read = None;
            cx.notify();
            return;
        };
        if self.load == Load::Failed {
            self.load = Load::Loading;
        }
        self.read_revision += 1;
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
        let (Some(target), Some(_)) = (self.target.clone(), self.load.ready()) else {
            return;
        };
        let client = target.client;
        let reading = NetworkTask::spawn_on(&target.network, async move { client.agents_md().await });
        cx.spawn(async move |this, cx| {
            let Some(Ok(file)) = reading.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
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
        self.load.ready().is_some() && self.target.is_some()
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
        let (Some(target), Some(loaded)) = (self.target.clone(), self.load.ready().cloned()) else {
            return;
        };
        let form = self.form(cx);
        let prompt_changed = form.prompt != loaded.saved.prompt;
        let settings_changed = form.enabled != loaded.saved.enabled
            || form.preset != loaded.saved.preset
            || form.custom != loaded.saved.custom;
        if !prompt_changed && !settings_changed {
            return;
        }
        if !self.save.begin() {
            return;
        }
        let content = if prompt_changed {
            normalize(&form.prompt)
        } else {
            loaded.saved.prompt.clone()
        };
        let changes = BehaviorChanges {
            global_behavior_prompt: prompt_changed.then(|| content.clone()),
            response_style_enabled: form.enabled,
            response_style_preset: form.preset.clone(),
            response_style_custom_instructions: form.custom.clone(),
        };
        let client = target.client.clone();
        let expected = loaded.on_disk.clone();
        let file_content = content.clone();
        let writing = NetworkTask::spawn_on(&target.network, async move {
            let mut file_written = false;
            if prompt_changed {
                match client
                    .save_agents_md(&file_content, expected.as_deref())
                    .await
                {
                    Ok(()) => file_written = true,
                    Err(AgentsMdError::Conflict) => {
                        return (file_written, Err(SaveFailure::ChangedOnDisk));
                    }
                    Err(error) => {
                        let failure = error
                            .message()
                            .filter(|message| !message.trim().is_empty())
                            .map_or(SaveFailure::Failed, |message| {
                                SaveFailure::Server(message.to_owned())
                            });
                        return (file_written, Err(failure));
                    }
                }
            }
            let result = client
                .save_behavior_settings(&changes)
                .await
                .map(|_| ())
                .map_err(|_| SaveFailure::Failed);
            (file_written, result)
        });
        let runtime = target.runtime.clone();
        self.write = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = writing.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.target.as_ref().map(|target| &target.runtime) != Some(&runtime) {
                    return;
                }
                this.write = None;
                let (file_written, result) =
                    outcome.unwrap_or((false, Err(SaveFailure::Failed)));
                if let Some(loaded) = this.load.ready_mut() {
                    if file_written {
                        loaded.on_disk = Some(content.clone());
                    }
                    if result.is_ok() {
                        loaded.saved = Saved {
                            prompt: content.clone(),
                            enabled: form.enabled,
                            preset: form.preset.clone(),
                            custom: form.custom.clone(),
                        };
                    }
                }
                // Normalize only the submitted draft; a newer edit survives.
                if file_written && this.prompt.read(cx).value().as_str() == form.prompt {
                    this.prompt
                        .update(cx, |input, cx| input.set_value(content.clone(), window, cx));
                }
                match result {
                    Ok(()) => this.failure = None,
                    Err(failure) => this.fail(failure, window, cx),
                }
                if this.save.finish() {
                    this.request_save(window, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
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
    fn server(file: Option<&str>, conflict: bool, settings_ok: bool) -> (Server, Arc<Mutex<Value>>) {
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
                    return (409, json!({"error": "changed", "code": "AGENTS_MD_CONFLICT"}));
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
        assert_eq!(document.lock().expect("settings")["responseStyleEnabled"], true);
    }
}
