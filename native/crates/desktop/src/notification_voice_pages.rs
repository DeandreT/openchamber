//! Settings' Notifications and Voice pages (`NotificationSettings.tsx`,
//! `VoiceSettings.tsx`'s speech recognition section). Their fields are the
//! shared server settings model's (`GeneralSettings`); this module owns
//! their text fields, the local speech model list and the voice API key.
//!
//! The browser-only push section and Voice's read-aloud section are not
//! drawn: native has no web push and no read-aloud.

use std::collections::HashMap;
use std::time::Duration;

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, Focusable, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Role, SharedString, StatefulInteractiveElement, Styled, Task,
    TestSupportExt, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::dictation::{DictationStatus, SttProvider};
use openchamber_native_transport::notification::{NotificationTemplate, NotificationTemplates};

use crate::activation::on_activate;
use crate::chrome::glyph;
use crate::desktop_notifications::Shown;
use crate::general_settings::{
    Choice, DICTATION_AUTO_SEND, DICTATION_ENABLED, NOTIFY_ALWAYS, NOTIFY_COMPLETION,
    NOTIFY_ENABLED, NOTIFY_ERROR, NOTIFY_PULL_REQUESTS, NOTIFY_QUESTION, NOTIFY_SUBTASKS,
    NOTIFY_TEMPLATES, STT_CORRECT, STT_ENDPOINT_PATH, STT_LANGUAGE, STT_LOCAL_MODEL, STT_MODEL,
    STT_PROVIDER, STT_SERVER_URL,
};
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::network::{Network, NetworkTask};
use crate::preference_pages::{CheckboxRow, Chip, PreferencePage};
use crate::settings_controls::{ServerTarget, SettingsWrites, info_hint, same_target, section};
use crate::theme::Palette;

/// The search targets these pages answer to.
pub(crate) const CONTROLS: [&str; 6] = [
    "notifications.delivery",
    "notifications.events",
    "notifications.pullRequests",
    "voice.speech-recognition",
    "voice.dictation-auto-send",
    "voice.stt-endpoint-path",
];

/// `LOCAL_STT_MODELS`: the catalog the model picker offers.
pub(crate) struct LocalSttModel {
    pub(crate) id: &'static str,
    label: &'static str,
    badge: Option<&'static str>,
    accuracy: u8,
    speed: u8,
    size: &'static str,
    /// The model takes the dictation language instead of detecting it.
    pub(crate) language_hint: bool,
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use crate::settings_controls::tests::retarget;
    use crate::settings_navigation::Page;
    use crate::settings_test_server::{Server, open, settings, settle};
    use gpui_kit::TestAppContext;
    use serde_json::json;
    use std::sync::Arc;

    fn held_key_server() -> (Server, Arc<crate::settings_controls::tests::ResponseGate>) {
        let gate = crate::settings_controls::tests::ResponseGate::new();
        let held = Arc::clone(&gate);
        let server = Server::new(move |request| {
            if request.is("PUT", "/api/voice/keys") {
                held.hold();
            }
            if request.is("GET", "/api/voice/keys") || request.is("PUT", "/api/voice/keys") {
                return (
                    200,
                    json!({"openai": false, "openaiCompatible": false, "stt": request.method == "PUT"}),
                );
            }
            (
                200,
                json!({"dictationEnabled": true, "sttProvider": "openai-compatible"}),
            )
        });
        (server, gate)
    }

    fn accept_key(
        window: gpui_kit::AnyWindowHandle,
        page: &Entity<PreferencePage>,
        cx: &mut TestAppContext,
    ) {
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.save_api_key(Some("synthetic-key-a".into()), window, cx)
            });
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn accepted_lifecycle_voice_write_keeps_its_close_receipt_after_a_host_switch(
        cx: &mut TestAppContext,
    ) {
        let (first, gate) = held_key_server();
        let (second, _) = held_key_server();
        let (window, workspace) = open(&first, Page::Voice, cx);
        let settings = settings(&workspace, cx);
        let page = cx.read(|cx| settings.read(cx).voice_for_test());
        accept_key(window, &page, cx);
        gate.entered(cx);
        retarget(&workspace, &second, cx);
        settle(window, cx);
        let pending = settings.update(cx, |settings, cx| settings.take_pending_write(cx));
        gate.release();
        assert!(
            pending.is_some(),
            "normal close still waits for the accepted A key write"
        );
        cx.foreground_executor().block_test(async move {
            pending.expect("close receipt").await;
        });
        assert_eq!(first.writes().len(), 1, "the captured A operation finishes");
        assert!(second.writes().is_empty(), "B receives no key operation");
    }

    #[gpui_kit::test]
    fn accepted_lifecycle_app_quit_drains_a_submitted_voice_key(cx: &mut TestAppContext) {
        let (server, gate) = held_key_server();
        let (window, workspace) = open(&server, Page::Voice, cx);
        let settings = settings(&workspace, cx);
        let page = cx.read(|cx| settings.read(cx).voice_for_test());
        accept_key(window, &page, cx);
        gate.entered(cx);
        // This test owns the standalone Settings drain. The workspace's real
        // quit observer must not consume that receipt before this observer.
        workspace.update(cx, |workspace, _| workspace.settings = None);
        cx.update(|cx| {
            cx.on_app_quit(move |cx| {
                let pending = settings.update(cx, |settings, cx| settings.take_quit_write(cx));
                gate.release();
                assert!(
                    pending.is_some(),
                    "quit retains the already submitted secret write"
                );
                async move {
                    pending.expect("quit drain").await;
                }
            })
            .detach();
        });
        cx.executor().allow_parking();
        cx.quit();
        assert_eq!(
            server.writes().len(),
            1,
            "quit waits for the accepted write's response"
        );
    }

    #[gpui_kit::test]
    fn workspace_quit_drains_a_submitted_voice_key(cx: &mut TestAppContext) {
        let (server, gate) = held_key_server();
        let (window, workspace) = open(&server, Page::Voice, cx);
        let settings = settings(&workspace, cx);
        let page = cx.read(|cx| settings.read(cx).voice_for_test());
        accept_key(window, &page, cx);
        gate.entered(cx);
        assert!(
            settings
                .update(cx, |settings, cx| settings.take_pending_write(cx))
                .is_some(),
            "normal close exposes the accepted key's receipt before quit takes custody"
        );
        cx.update(|cx| {
            cx.on_app_quit(move |_| {
                gate.release();
                async {}
            })
            .detach();
        });
        cx.executor().allow_parking();
        cx.quit();
        assert_eq!(
            server.writes().len(),
            1,
            "the production workspace quit hook drains the submitted key exactly once"
        );
    }

    #[gpui_kit::test]
    fn accepted_lifecycle_quit_never_submits_a_merely_typed_secret(cx: &mut TestAppContext) {
        let (server, _) = held_key_server();
        let (window, workspace) = open(&server, Page::Voice, cx);
        let settings = settings(&workspace, cx);
        let page = cx.read(|cx| settings.read(cx).voice_for_test());
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                let input = &page
                    .extra
                    .inputs
                    .get("voice-stt-api-key")
                    .expect("key input")
                    .1;
                input.update(cx, |input, cx| {
                    input.set_value("synthetic-unsubmitted", window, cx)
                });
            });
            // Quit captures ordinary text, but must never accept a secret implicitly.
            settings.update(cx, |settings, cx| settings.prepare_close(window, cx));
        })
        .expect("test window");
        cx.update(|cx| {
            cx.on_app_quit(move |cx| {
                let pending = settings.update(cx, |settings, cx| settings.take_quit_write(cx));
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
        assert!(
            server.writes().is_empty(),
            "a typed secret never enters a quit write"
        );
    }

    #[gpui_kit::test]
    fn review_regression_a_failed_voice_key_never_follows_a_host_switch(cx: &mut TestAppContext) {
        let first = Server::new(|request| {
            if request.is("PUT", "/api/voice/keys") {
                return (500, json!({}));
            }
            if request.is("GET", "/api/voice/keys") {
                return (
                    200,
                    json!({"openai": false, "openaiCompatible": false, "stt": false}),
                );
            }
            (
                200,
                json!({"dictationEnabled": true, "sttProvider": "openai-compatible"}),
            )
        });
        let second = Server::new(|request| {
            if request.is("GET", "/api/voice/keys") {
                return (
                    200,
                    json!({"openai": false, "openaiCompatible": false, "stt": false}),
                );
            }
            (
                200,
                json!({"dictationEnabled": true, "sttProvider": "openai-compatible"}),
            )
        });
        let (window, workspace) = open(&first, Page::Voice, cx);
        let settings = settings(&workspace, cx);
        // Instantiate the same retained secret field the Voice page uses.
        let page = cx.read(|cx| settings.read(cx).voice_for_test());
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                let _ = page.text_input(
                    "voice-stt-api-key",
                    Field::ApiKey,
                    "".into(),
                    true,
                    window,
                    cx,
                );
                let input = page
                    .extra
                    .inputs
                    .get("voice-stt-api-key")
                    .expect("key input")
                    .1
                    .clone();
                input.update(cx, |input, cx| {
                    input.set_value("synthetic-key-a", window, cx)
                });
                page.save_api_key(Some("synthetic-key-a".into()), window, cx);
            });
        })
        .expect("test window");
        settle(window, cx);
        retarget(&workspace, &second, cx);
        settle(window, cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                let input = page
                    .extra
                    .inputs
                    .get("voice-stt-api-key")
                    .expect("key input")
                    .1
                    .clone();
                input.update(cx, |_, cx| cx.emit(InputEvent::Blur));
                let _ = window;
            });
        })
        .expect("test window");
        settle(window, cx);
        assert!(
            second.writes().is_empty(),
            "blurring B never transmits A's retained key"
        );
    }
}

pub(crate) const LOCAL_STT_MODELS: [LocalSttModel; 5] = [
    LocalSttModel {
        id: "parakeet-tdt-0.6b-v2-int8",
        label: "settings.voice.page.stt.model.parakeetV2",
        badge: Some("settings.voice.page.stt.badge.bestForEnglish"),
        accuracy: 5,
        speed: 5,
        size: "460 MB",
        language_hint: false,
    },
    LocalSttModel {
        id: "parakeet-tdt-0.6b-v3-int8",
        label: "settings.voice.page.stt.model.parakeetV3",
        badge: Some("settings.voice.page.stt.badge.bestForMultilingual"),
        accuracy: 5,
        speed: 4,
        size: "465 MB",
        language_hint: false,
    },
    LocalSttModel {
        id: "whisper-large-v3-turbo-int8",
        label: "settings.voice.page.stt.model.whisperTurbo",
        badge: None,
        accuracy: 4,
        speed: 1,
        size: "1 GB",
        language_hint: true,
    },
    LocalSttModel {
        id: "qwen3-asr-0.6b-int8",
        label: "settings.voice.page.stt.model.qwen3Asr",
        badge: None,
        accuracy: 4,
        speed: 3,
        size: "1 GB",
        language_hint: true,
    },
    LocalSttModel {
        id: "whisper-tiny-int8",
        label: "settings.voice.page.stt.model.whisperTiny",
        badge: None,
        accuracy: 2,
        speed: 4,
        size: "115 MB",
        language_hint: true,
    },
];

/// The four events with custom text, in the page's order.
const TEMPLATE_EVENTS: [(&str, &str, &str, &str); 4] = [
    (
        "completion",
        "settings.notifications.page.template.event.completion",
        "settings.notifications.page.template.defaults.completion.title",
        "settings.notifications.page.template.defaults.completion.message",
    ),
    (
        "subtask",
        "settings.notifications.page.template.event.subtask",
        "settings.notifications.page.template.defaults.subtask.title",
        "settings.notifications.page.template.defaults.subtask.message",
    ),
    (
        "error",
        "settings.notifications.page.template.event.error",
        "settings.notifications.page.template.defaults.error.title",
        "settings.notifications.page.template.defaults.error.message",
    ),
    (
        "question",
        "settings.notifications.page.template.event.question",
        "settings.notifications.page.template.defaults.question.title",
        "settings.notifications.page.template.defaults.question.message",
    ),
];

fn template_mut<'a>(
    templates: &'a mut NotificationTemplates,
    event: &str,
) -> &'a mut NotificationTemplate {
    match event {
        "completion" => &mut templates.completion,
        "subtask" => &mut templates.subtask,
        "error" => &mut templates.error,
        _ => &mut templates.question,
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// A text field and the setting it edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Setting(usize),
    Template {
        event: &'static str,
        message: bool,
    },
    /// The OpenAI-compatible transcription key, kept on the server.
    ApiKey,
}

/// The local model list as last read, and the work around it.
#[derive(Default)]
pub(crate) struct PageState {
    target: Option<ServerTarget>,
    network: Option<Network>,
    writes: SettingsWrites,
    inputs: HashMap<SharedString, (Field, Entity<InputState>)>,
    models: Option<DictationStatus>,
    models_failed: bool,
    models_read: Option<Task<()>>,
    /// The model a download or delete runs for.
    requesting: Option<(String, Task<()>)>,
    /// Whether a transcription key is saved; unknown until read.
    key_saved: Option<bool>,
    key_task: Option<Task<()>>,
    key_saving: bool,
    key_revision: u64,
    key_failed: bool,
    test: Option<Task<()>>,
}

impl PreferencePage {
    pub(crate) fn observe_voice_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.extra.network = Network::new().ok();
        if let Some(owner) = self.owner.upgrade() {
            self._subscriptions
                .push(cx.observe_in(&owner, window, |this, _, window, cx| {
                    this.sync_voice_target(window, cx);
                }));
        }
    }

    fn voice_target(&self, cx: &gpui_kit::App) -> Option<ServerTarget> {
        let owner = self.owner.upgrade()?;
        let owner = owner.read(cx);
        let connection = owner.connection.as_ref()?;
        Some(ServerTarget {
            runtime: owner.runtime.clone(),
            generation: connection.generation,
            client: connection.client.clone()?,
            network: self.extra.network.as_ref()?.handle(),
        })
    }

    fn sync_voice_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.voice_target(cx);
        if same_target(self.extra.target.as_ref(), target.as_ref()) {
            return;
        }
        self.extra.target = target;
        self.extra.key_task = None;
        self.extra.key_saved = None;
        self.extra.key_saving = false;
        self.extra.key_failed = false;
        self.extra.key_revision += 1;
        if let Some((_, input)) = self.extra.inputs.get("voice-stt-api-key") {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.extra.models = None;
        self.extra.models_failed = false;
        self.extra.models_read = None;
        self.extra.requesting = None;
        cx.notify();
    }

    fn connected(&self, cx: &gpui_kit::App) -> Option<(ServerClient, tokio::runtime::Handle)> {
        let owner = self.owner.upgrade()?;
        let connection = owner.read(cx).connection.as_ref()?;
        Some((connection.client.clone()?, connection.network.handle()))
    }

    fn boolean(&self, field: usize, cx: &gpui_kit::App) -> bool {
        matches!(
            self.general.read(cx).current_choice(field),
            Choice::Boolean(true)
        )
    }

    fn text_value(&self, field: Field, cx: &gpui_kit::App) -> String {
        let general = self.general.read(cx);
        match field {
            Field::Setting(field) => match general.current_choice(field) {
                Choice::Text(text) => text,
                _ => String::new(),
            },
            Field::Template { event, message } => match general.current_choice(NOTIFY_TEMPLATES) {
                Choice::Templates(mut templates) => {
                    let template = template_mut(&mut templates, event);
                    if message {
                        template.message.clone()
                    } else {
                        template.title.clone()
                    }
                }
                _ => String::new(),
            },
            Field::ApiKey => String::new(),
        }
    }

    fn text_changed(&mut self, field: Field, value: String, cx: &mut Context<Self>) {
        match field {
            Field::Setting(field) => self.change(field, Choice::Text(value), cx),
            Field::Template { event, message } => {
                let Choice::Templates(mut templates) =
                    self.general.read(cx).current_choice(NOTIFY_TEMPLATES)
                else {
                    return;
                };
                let template = template_mut(&mut templates, event);
                if message {
                    template.message = value;
                } else {
                    template.title = value;
                }
                self.change(NOTIFY_TEMPLATES, Choice::Templates(templates), cx);
            }
            Field::ApiKey => {}
        }
    }

    /// The field's input, created on first use, showing the setting's value
    /// unless the user is typing in it.
    fn text_input(
        &mut self,
        id: &'static str,
        field: Field,
        placeholder: SharedString,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = SharedString::from(id);
        let input = match self.extra.inputs.get(&key) {
            Some((_, input)) => input.clone(),
            None => {
                let input = cx.new(|cx| InputState::new(window, cx).masked(field == Field::ApiKey));
                let subscription = cx.subscribe_in(
                    &input,
                    window,
                    move |this, input, event: &InputEvent, window, cx| match (event, field) {
                        (InputEvent::Change, Field::ApiKey) => {
                            this.extra.key_revision += 1;
                            cx.notify();
                        }
                        (InputEvent::Change, _) => {
                            let value = input.read(cx).value().to_string();
                            this.text_changed(field, value, cx);
                        }
                        (InputEvent::PressEnter { .. } | InputEvent::Blur, Field::ApiKey) => {
                            this.sync_voice_target(window, cx);
                            let value = input.read(cx).value().trim().to_owned();
                            if !value.is_empty() {
                                this.save_api_key(Some(value), window, cx);
                            }
                        }
                        _ => {}
                    },
                );
                self._subscriptions.push(subscription);
                self.extra.inputs.insert(key, (field, input.clone()));
                input
            }
        };
        let value = self.text_value(field, cx);
        let focused = input.read(cx).focus_handle(cx).is_focused(window);
        input.update(cx, |input, cx| {
            if &placeholder != input.presentation().placeholder() {
                input.set_placeholder(placeholder.clone(), window, cx);
            }
            if field != Field::ApiKey && !focused && input.value().as_ref() != value {
                input.set_value(value.clone(), window, cx);
            }
        });
        div()
            .w(px(320.0))
            .max_w_full()
            .h(gpui_kit::rems(2.0))
            .when(!enabled, |row| row.opacity(0.5))
            .child(
                Input::new(&input)
                    .id(SharedString::from(id))
                    .accessibility_id(id)
                    .disabled(!enabled)
                    .w_full()
                    .h(gpui_kit::rems(2.0)),
            )
            .into_any_element()
    }

    /// A label above its control, as `SettingsStackedField` lays them out.
    fn stacked(
        &self,
        item: &'static str,
        label: SharedString,
        info: Option<SharedString>,
        error: bool,
        control: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = cx.global::<Palette>().clone();
        let direction = locale::strings(cx).direction();
        div()
            .id(SharedString::from(format!(
                "{}-field",
                item.replace('.', "-")
            )))
            .anchor_scroll(self.anchor(item))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .py(px(4.0))
            .child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .when(error, |label| label.text_color(palette.status_error))
                    .child(label)
                    .children(info.map(|info| info_hint(item, info, cx))),
            )
            .child(control)
            .into_any_element()
    }

    pub(crate) fn notifications(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let strings = locale::strings(cx).clone();
        let on = self.boolean(NOTIFY_ENABLED, cx);
        let delivery = self.checkbox(
            CheckboxRow {
                item: "notifications.delivery",
                label: "settings.notifications.page.delivery.enableLabel",
                aria: "settings.notifications.page.delivery.enableAria",
                info: None,
                checked: on,
                enabled,
                on_change: |this, value, cx| {
                    this.change(NOTIFY_ENABLED, Choice::Boolean(value), cx)
                },
            },
            cx,
        );
        let mut rows = vec![delivery];
        if on {
            rows.push(self.checkbox(
                CheckboxRow {
                    item: "notifications.focused",
                    label: "settings.notifications.page.delivery.focusedLabel",
                    aria: "settings.notifications.page.delivery.focusedAria",
                    info: None,
                    checked: self.boolean(NOTIFY_ALWAYS, cx),
                    enabled,
                    on_change: |this, value, cx| {
                        this.change(NOTIFY_ALWAYS, Choice::Boolean(value), cx)
                    },
                },
                cx,
            ));
            let label = strings.get("settings.notifications.page.delivery.testAction");
            rows.push(
                div()
                    .py(px(8.0))
                    .flex_row_from(strings.direction())
                    .child(on_activate(
                        gpui_kit::base::Button::new("notifications-test")
                            .accessibility_id("notifications-test")
                            .accessibility_label(label.clone())
                            .h(gpui_kit::rems(1.75))
                            .px(px(10.0))
                            .rounded(px(7.0))
                            .child(label),
                        cx,
                        |this, window, cx| this.test_notification(window, cx),
                    ))
                    .into_any_element(),
            );
        }
        let mut sections = vec![section(
            "settings.notifications.page.delivery.title",
            true,
            rows,
            cx,
        )];
        if !on {
            return sections;
        }
        type Toggle = (
            &'static str,
            &'static str,
            &'static str,
            Option<&'static str>,
            usize,
            fn(&mut PreferencePage, bool, &mut Context<PreferencePage>),
        );
        let toggles: [Toggle; 5] = [
            (
                "notifications.completion",
                "settings.notifications.page.events.completionLabel",
                "settings.notifications.page.events.completionAria",
                None,
                NOTIFY_COMPLETION,
                |this, value, cx| this.change(NOTIFY_COMPLETION, Choice::Boolean(value), cx),
            ),
            (
                "notifications.pullRequests",
                "settings.notifications.pullRequests.label",
                "settings.notifications.pullRequests.label",
                Some("settings.notifications.pullRequests.info"),
                NOTIFY_PULL_REQUESTS,
                |this, value, cx| this.change(NOTIFY_PULL_REQUESTS, Choice::Boolean(value), cx),
            ),
            (
                "notifications.subtasks",
                "settings.notifications.page.events.subtaskLabel",
                "settings.notifications.page.events.subtaskAria",
                None,
                NOTIFY_SUBTASKS,
                |this, value, cx| this.change(NOTIFY_SUBTASKS, Choice::Boolean(value), cx),
            ),
            (
                "notifications.errors",
                "settings.notifications.page.events.errorLabel",
                "settings.notifications.page.events.errorAria",
                None,
                NOTIFY_ERROR,
                |this, value, cx| this.change(NOTIFY_ERROR, Choice::Boolean(value), cx),
            ),
            (
                "notifications.questions",
                "settings.notifications.page.events.questionLabel",
                "settings.notifications.page.events.questionAria",
                None,
                NOTIFY_QUESTION,
                |this, value, cx| this.change(NOTIFY_QUESTION, Choice::Boolean(value), cx),
            ),
        ];
        let mut events = Vec::new();
        for (item, label, aria, info, field, on_change) in toggles {
            let checked = self.boolean(field, cx);
            events.push(self.checkbox(
                CheckboxRow {
                    item,
                    label,
                    aria,
                    info,
                    checked,
                    enabled,
                    on_change,
                },
                cx,
            ));
        }
        sections.push(
            div()
                .id("notifications-events")
                .anchor_scroll(self.anchor("notifications.events"))
                .child(section(
                    "settings.notifications.page.events.title",
                    false,
                    events,
                    cx,
                ))
                .into_any_element(),
        );
        let palette = cx.global::<Palette>().clone();
        let mut groups = Vec::new();
        for (event, label, title_default, message_default) in TEMPLATE_EVENTS {
            let title = self.text_input(
                match event {
                    "completion" => "notifications-template-completion-title",
                    "subtask" => "notifications-template-subtask-title",
                    "error" => "notifications-template-error-title",
                    _ => "notifications-template-question-title",
                },
                Field::Template {
                    event,
                    message: false,
                },
                strings.get(title_default),
                enabled,
                window,
                cx,
            );
            let message = self.text_input(
                match event {
                    "completion" => "notifications-template-completion-message",
                    "subtask" => "notifications-template-subtask-message",
                    "error" => "notifications-template-error-message",
                    _ => "notifications-template-question-message",
                },
                Field::Template {
                    event,
                    message: true,
                },
                strings.get(message_default),
                enabled,
                window,
                cx,
            );
            let caption = |key: &'static str| {
                div()
                    .text_color(palette.muted_foreground)
                    .text_size(gpui_kit::rems(0.75))
                    .child(strings.get(key))
            };
            groups.push(
                div()
                    .p(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "notifications-template-{event}"
                            )))
                            .role(Role::Heading)
                            .font_weight(FontWeight::MEDIUM)
                            // The web capitalizes the event's name with CSS.
                            .child(capitalized(&strings.get(label))),
                    )
                    .child(caption("settings.notifications.page.template.field.title"))
                    .child(title)
                    .child(caption(
                        "settings.notifications.page.template.field.message",
                    ))
                    .child(message)
                    .into_any_element(),
            );
        }
        let variables = format!(
            "{} {{project_name}} {{worktree}} {{branch}} {{session_name}} {{agent_name}} {{model_name}} {{last_message}}",
            strings.get("settings.notifications.page.template.variablesLabel")
        );
        let mut rows = vec![
            div()
                .text_color(palette.muted_foreground)
                .child(variables)
                .into_any_element(),
        ];
        rows.extend(groups);
        sections.push(section(
            "settings.notifications.page.template.title",
            false,
            rows,
            cx,
        ));
        sections
    }

    /// `handleTestNotification`: shows one notification right away.
    fn test_notification(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let strings = locale::strings(cx);
        let shown = Shown {
            title: strings
                .get("settings.notifications.page.testNotification.title")
                .to_string(),
            body: strings
                .get("settings.notifications.page.testNotification.body")
                .to_string(),
            action: strings
                .get("chat.toast.opencodeRestartInterrupted.openSession")
                .to_string(),
        };
        let shown_task = owner.update(cx, |owner, cx| {
            owner.show_desktop_notification(shown, None, window, cx)
        });
        self.extra.test = Some(cx.spawn_in(window, async move |_, cx| {
            let delivered = shown_task.await;
            let _ = cx.update(|window, cx| {
                let strings = locale::strings(cx);
                let notification = if delivered {
                    Notification::success(
                        strings.get("settings.notifications.page.toast.testNotificationSent"),
                    )
                } else {
                    Notification::error(
                        strings.get("settings.notifications.page.toast.testNotificationFailed"),
                    )
                };
                window.push_notification(notification, cx);
            });
        }));
    }

    pub(crate) fn voice(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.sync_voice_target(window, cx);
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let dictation = self.boolean(DICTATION_ENABLED, cx);
        let mut rows = vec![
            self.checkbox(
                CheckboxRow {
                    item: "voice.speech-recognition",
                    label: "settings.voice.page.field.enableVoiceInput",
                    aria: "settings.voice.page.field.enableVoiceInputAria",
                    info: None,
                    checked: dictation,
                    enabled,
                    on_change: |this, value, cx| {
                        this.change(DICTATION_ENABLED, Choice::Boolean(value), cx)
                    },
                },
                cx,
            ),
            self.checkbox(
                CheckboxRow {
                    item: "voice.dictation-auto-send",
                    label: "settings.voice.page.field.dictationAutoSend",
                    aria: "settings.voice.page.field.dictationAutoSendAria",
                    info: Some("settings.voice.page.field.dictationAutoSendInfo"),
                    checked: self.boolean(DICTATION_AUTO_SEND, cx),
                    enabled,
                    on_change: |this, value, cx| {
                        this.change(DICTATION_AUTO_SEND, Choice::Boolean(value), cx)
                    },
                },
                cx,
            ),
        ];
        if dictation {
            let Choice::Provider(provider) = self.general.read(cx).current_choice(STT_PROVIDER)
            else {
                unreachable!("the provider field holds a provider");
            };
            let chips = [
                (
                    SttProvider::Local,
                    "local",
                    "settings.voice.page.provider.local",
                ),
                (
                    SttProvider::OpenAiCompatible,
                    "openai-compatible",
                    "settings.voice.page.provider.server",
                ),
            ]
            .into_iter()
            .map(|(value, name, label)| Chip {
                id: format!("voice.stt-provider-{name}").into(),
                label: strings.get(label),
                icon: None,
                tooltip: None,
                selected: value == provider,
                disabled: !enabled,
                choice: Choice::Provider(value),
            })
            .collect();
            let info: SharedString = format!(
                "{} {}\n{} {}",
                strings.get("settings.voice.page.provider.local"),
                strings.get("settings.voice.page.tooltip.sttLocal"),
                strings.get("settings.voice.page.provider.server"),
                strings.get("settings.voice.page.tooltip.sttServer"),
            )
            .into();
            rows.push(self.chips(
                "voice.stt-provider",
                "settings.voice.page.field.provider",
                Some(info),
                chips,
                STT_PROVIDER,
                cx,
            ));
            match provider {
                SttProvider::Local => {
                    self.read_models_once(cx);
                    let picker = self.model_picker(enabled, cx);
                    rows.push(self.stacked(
                        "voice.stt-model",
                        strings.get("settings.voice.page.field.model"),
                        None,
                        false,
                        picker,
                        cx,
                    ));
                    let saved = self.text_value(Field::Setting(STT_LOCAL_MODEL), cx);
                    if LOCAL_STT_MODELS
                        .iter()
                        .any(|model| model.id == saved && model.language_hint)
                    {
                        let input = self.text_input(
                            "voice-stt-language",
                            Field::Setting(STT_LANGUAGE),
                            "auto".into(),
                            enabled,
                            window,
                            cx,
                        );
                        rows.push(self.stacked(
                            "voice.stt-language",
                            strings.get("settings.voice.page.field.language"),
                            Some(strings.get("settings.voice.page.field.sttLanguageHint")),
                            false,
                            input,
                            cx,
                        ));
                    }
                }
                SttProvider::OpenAiCompatible => {
                    let url_empty = self
                        .text_value(Field::Setting(STT_SERVER_URL), cx)
                        .trim()
                        .is_empty();
                    let url = self.text_input(
                        "voice-stt-server-url",
                        Field::Setting(STT_SERVER_URL),
                        "http://localhost:8001/v1".into(),
                        enabled,
                        window,
                        cx,
                    );
                    rows.push(self.stacked(
                        "voice.stt-server-url",
                        strings.get("settings.voice.page.field.serverUrl"),
                        Some(strings.get("settings.voice.page.field.sttServerUrlHint")),
                        url_empty,
                        url,
                        cx,
                    ));
                    self.read_key_once(cx);
                    let placeholder: SharedString = if self.extra.key_saved == Some(true) {
                        strings.get("settings.voice.page.field.apiKeySavedPlaceholder")
                    } else {
                        "sk-...".into()
                    };
                    let key = self.text_input(
                        "voice-stt-api-key",
                        Field::ApiKey,
                        placeholder,
                        enabled && !self.extra.key_saving && !self.extra.writes.closing(),
                        window,
                        cx,
                    );
                    let remove = (self.extra.key_saved == Some(true)).then(|| {
                        let label = strings.get("settings.voice.page.field.apiKeyRemove");
                        on_activate(
                            div()
                                .id("voice-stt-api-key-remove")
                                .accessibility_id("voice-stt-api-key-remove")
                                .role(Role::Button)
                                .aria_label(label)
                                .cursor_pointer()
                                .child(glyph("close", 14.0, palette.muted_foreground)),
                            cx,
                            |this, window, cx| this.save_api_key(None, window, cx),
                        )
                        .test_support()
                    });
                    let key = div()
                        .flex_row_from(strings.direction())
                        .items_center()
                        .gap(px(8.0))
                        .child(key)
                        .children(remove)
                        .child(
                            div()
                                .text_color(palette.muted_foreground)
                                .child(strings.get("settings.voice.page.field.apiKeyOptional")),
                        )
                        .into_any_element();
                    rows.push(self.stacked(
                        "voice.stt-api-key",
                        strings.get("settings.voice.page.field.apiKey"),
                        None,
                        false,
                        key,
                        cx,
                    ));
                    let model = self.text_input(
                        "voice-stt-model",
                        Field::Setting(STT_MODEL),
                        "deepdml/faster-whisper-large-v3-turbo-ct2".into(),
                        enabled,
                        window,
                        cx,
                    );
                    rows.push(self.stacked(
                        "voice.stt-remote-model",
                        strings.get("settings.voice.page.field.model"),
                        None,
                        false,
                        model,
                        cx,
                    ));
                    let path = self.text_input(
                        "voice-stt-endpoint-path",
                        Field::Setting(STT_ENDPOINT_PATH),
                        "/audio/transcriptions".into(),
                        enabled,
                        window,
                        cx,
                    );
                    rows.push(self.stacked(
                        "voice.stt-endpoint-path",
                        strings.get("settings.voice.page.field.endpointPath"),
                        Some(strings.get("settings.voice.page.field.endpointPathHint")),
                        false,
                        path,
                        cx,
                    ));
                    let language = self.text_input(
                        "voice-stt-language",
                        Field::Setting(STT_LANGUAGE),
                        "auto".into(),
                        enabled,
                        window,
                        cx,
                    );
                    rows.push(self.stacked(
                        "voice.stt-language",
                        strings.get("settings.voice.page.field.language"),
                        Some(strings.get("settings.voice.page.field.sttLanguageHint")),
                        false,
                        language,
                        cx,
                    ));
                }
            }
            rows.push(self.checkbox(
                CheckboxRow {
                    item: "voice.correct-transcript",
                    label: "settings.voice.page.field.correctTranscript",
                    aria: "settings.voice.page.field.correctTranscript",
                    info: Some("settings.voice.page.field.correctTranscriptInfo"),
                    checked: self.boolean(STT_CORRECT, cx),
                    enabled,
                    on_change: |this, value, cx| {
                        this.change(STT_CORRECT, Choice::Boolean(value), cx)
                    },
                },
                cx,
            ));
        }
        vec![section(
            "settings.voice.page.section.speechRecognition",
            true,
            rows,
            cx,
        )]
    }

    /// The model the picker shows as chosen: the saved one, else the one
    /// the server picked (`activeModel`).
    fn selected_model(&self, cx: &gpui_kit::App) -> Option<String> {
        let saved = self.text_value(Field::Setting(STT_LOCAL_MODEL), cx);
        if LOCAL_STT_MODELS.iter().any(|model| model.id == saved) {
            return Some(saved);
        }
        self.extra
            .models
            .as_ref()
            .and_then(|status| status.active_model.clone())
    }

    fn read_models_once(&mut self, cx: &mut Context<Self>) {
        if self.extra.models.is_none()
            && self.extra.models_read.is_none()
            && !self.extra.models_failed
        {
            self.read_models(Duration::ZERO, cx);
        }
    }

    /// Reads the model list; again every two seconds while one downloads.
    fn read_models(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let Some((client, network)) = self.connected(cx) else {
            return;
        };
        let saved = self.text_value(Field::Setting(STT_LOCAL_MODEL), cx);
        self.extra.models_read =
            Some(cx.spawn(async move |this, cx| {
                if !delay.is_zero() {
                    cx.background_executor().timer(delay).await;
                }
                let read = NetworkTask::spawn_on(&network, async move {
                    client.dictation_status(&saved).await
                });
                let result = read.await;
                let _ = this.update(cx, |this, cx| {
                    this.extra.models_read = None;
                    match result {
                        Some(Ok(status)) => {
                            let downloading = status.models.iter().any(|model| model.downloading);
                            this.extra.models = Some(status);
                            this.extra.models_failed = false;
                            if downloading {
                                this.read_models(Duration::from_secs(2), cx);
                            }
                        }
                        // Display-only: a failed read keeps the last list.
                        _ => this.extra.models_failed = true,
                    }
                    cx.notify();
                });
            }));
    }

    fn change_model(&mut self, model: &'static str, download: bool, cx: &mut Context<Self>) {
        let Some((client, network)) = self.connected(cx) else {
            return;
        };
        let request = NetworkTask::spawn_on(&network, async move {
            client.change_dictation_model(model, download).await
        });
        self.extra.requesting = Some((
            model.to_owned(),
            cx.spawn(async move |this, cx| {
                let _ = request.await;
                let _ = this.update(cx, |this, cx| {
                    this.extra.requesting = None;
                    this.read_models(Duration::ZERO, cx);
                });
            }),
        ));
        cx.notify();
    }

    fn model_picker(&mut self, enabled: bool, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let selected = self.selected_model(cx);
        let requesting = self
            .extra
            .requesting
            .as_ref()
            .map(|(model, _)| model.clone());
        let mut list = div()
            .id("voice-stt-models")
            .role(Role::RadioGroup)
            .aria_label(strings.get("settings.voice.page.field.model"))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(8.0));
        for model in &LOCAL_STT_MODELS {
            let chosen = selected.as_deref() == Some(model.id);
            let state = self
                .extra
                .models
                .as_ref()
                .and_then(|status| status.models.iter().find(|entry| entry.id == model.id))
                .cloned();
            let id = model.id;
            let label = strings.get(model.label);
            let bar = |name: &'static str, value: u8| {
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(gpui_kit::rems(0.75))
                            .text_color(palette.muted_foreground)
                            .child(strings.get(name)),
                    )
                    .child(
                        div()
                            .w(px(32.0))
                            .h(px(6.0))
                            .rounded(px(3.0))
                            .bg(palette.border)
                            .child(
                                div()
                                    .w(px(32.0 * f32::from(value) / 5.0))
                                    .h_full()
                                    .rounded(px(3.0))
                                    .bg(palette.primary),
                            ),
                    )
            };
            let busy = requesting.as_deref() == Some(id);
            let action: AnyElement = match &state {
                Some(state) if state.installed => div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(4.0))
                    .child(glyph("checkbox-circle", 16.0, palette.status_success))
                    .child(
                        on_activate(
                            div()
                                .id(SharedString::from(format!("voice-model-delete-{id}")))
                                .accessibility_id(format!("voice-model-delete-{id}"))
                                .role(Role::Button)
                                .aria_label(strings.get("settings.voice.page.stt.modelDelete"))
                                .when(!busy, |button| button.cursor_pointer())
                                .child(glyph("delete-bin", 16.0, palette.muted_foreground)),
                            cx,
                            move |this, _, cx| {
                                if this.extra.requesting.is_none() {
                                    this.change_model(id, false, cx);
                                }
                            },
                        )
                        .test_support(),
                    )
                    .into_any_element(),
                Some(state) if state.downloading => div()
                    .text_color(palette.muted_foreground)
                    .child(
                        state
                            .download_progress
                            .map(|percent| format!("{}%", percent.round()))
                            .unwrap_or_default(),
                    )
                    .into_any_element(),
                _ => on_activate(
                    div()
                        .id(SharedString::from(format!("voice-model-download-{id}")))
                        .accessibility_id(format!("voice-model-download-{id}"))
                        .role(Role::Button)
                        .aria_label(strings.get("settings.voice.page.stt.modelDownload"))
                        .when(!busy, |button| button.cursor_pointer())
                        .child(glyph("download", 16.0, palette.foreground)),
                    cx,
                    move |this, _, cx| {
                        if this.extra.requesting.is_none() {
                            this.change_model(id, true, cx);
                        }
                    },
                )
                .test_support()
                .into_any_element(),
            };
            let error = state.and_then(|state| state.download_error);
            let tile = on_activate(
                div()
                    .id(SharedString::from(format!("voice-model-{id}")))
                    .accessibility_id(format!("voice-model-{id}"))
                    .role(Role::RadioButton)
                    .aria_label(label.clone())
                    .aria_toggled(if chosen {
                        gpui_kit::Toggled::True
                    } else {
                        gpui_kit::Toggled::False
                    })
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .when(enabled, |tile| tile.cursor_pointer())
                    .child(label)
                    .children(model.badge.map(|badge| {
                        div()
                            .text_size(gpui_kit::rems(0.625))
                            .text_color(palette.status_success)
                            .child(strings.get(badge))
                    }))
                    .child(
                        div()
                            .flex_row_from(direction)
                            .flex_wrap()
                            .items_center()
                            .gap(px(12.0))
                            .child(bar("settings.voice.page.stt.meta.accuracy", model.accuracy))
                            .child(bar("settings.voice.page.stt.meta.speed", model.speed))
                            .child(
                                div()
                                    .text_size(gpui_kit::rems(0.75))
                                    .text_color(palette.muted_foreground)
                                    .child(model.size),
                            ),
                    )
                    .children(
                        error.map(|error| div().text_color(palette.status_error).child(error)),
                    ),
                cx,
                move |this, _, cx| {
                    if this.general.read(cx).editable() {
                        this.change(STT_LOCAL_MODEL, Choice::Text(id.to_owned()), cx);
                        this.read_models(Duration::ZERO, cx);
                    }
                },
            )
            .test_support();
            list = list.child(
                div()
                    .flex_row_from(direction)
                    .items_start()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .py(px(10.0))
                    .rounded(px(8.0))
                    .bg(if chosen {
                        palette.selection
                    } else {
                        palette.elevated
                    })
                    .when(chosen, |tile| tile.text_color(palette.selection_foreground))
                    .child(tile)
                    .child(action),
            );
        }
        list.into_any_element()
    }

    fn read_key_once(&mut self, cx: &mut Context<Self>) {
        if self.extra.key_saved.is_some() || self.extra.key_task.is_some() || self.extra.key_saving
        {
            return;
        }
        let Some(target) = self.extra.target.clone() else {
            return;
        };
        let client = target.client.clone();
        let read = NetworkTask::spawn_on(&target.network, async move { client.voice_keys().await });
        self.extra.key_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                if !same_target(this.extra.target.as_ref(), Some(&target))
                    || !same_target(this.voice_target(cx).as_ref(), Some(&target))
                {
                    return;
                }
                this.extra.key_task = None;
                if let Some(Ok(keys)) = result {
                    this.extra.key_saved = Some(keys.stt);
                }
                cx.notify();
            });
        }));
    }

    /// Saves the key typed into the field, or removes the saved one.
    fn save_api_key(&mut self, key: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.voice_target(cx);
        if !same_target(self.extra.target.as_ref(), current.as_ref()) {
            self.sync_voice_target(window, cx);
            return;
        }
        if self.extra.key_saving || self.extra.writes.closing() {
            return;
        }
        let Some(target) = current else {
            return;
        };
        let client = target.client.clone();
        let revision = self.extra.key_revision;
        self.extra.key_task = None;
        self.extra.key_saving = true;
        let (id, save) = self.extra.writes.submit(&target, async move {
            client.set_voice_stt_key(key.as_deref()).await
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result = save.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.extra.writes.finish(id);
                if !same_target(this.extra.target.as_ref(), Some(&target))
                    || !same_target(this.voice_target(cx).as_ref(), Some(&target))
                {
                    return;
                }
                this.extra.key_saving = false;
                this.extra.key_failed = !matches!(result, Some(Ok(_)));
                match result {
                    Some(Ok(keys)) => {
                        this.extra.key_saved = Some(keys.stt);
                        // The typed key leaves the field once the server has it.
                        if this.extra.key_revision == revision
                            && let Some((_, input)) = this.extra.inputs.get("voice-stt-api-key")
                        {
                            input.update(cx, |input, cx| input.set_value("", window, cx));
                        }
                    }
                    _ => window.push_notification(
                        Notification::error(
                            locale::strings(cx).get("settings.voice.page.toast.apiKeySaveFailed"),
                        ),
                        cx,
                    ),
                }
                cx.notify();
            });
        });
        self.extra.writes.observe(id, completion);
        cx.notify();
    }

    pub(crate) fn take_voice_pending_write(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let pending = self.extra.writes.take_pending_write();
        cx.notify();
        pending
    }

    pub(crate) fn voice_has_failed_writes(&self) -> bool {
        self.extra.key_failed
    }

    pub(crate) fn cancel_voice_close(&mut self, cx: &mut Context<Self>) {
        self.extra.writes.cancel_close();
        cx.notify();
    }

    pub(crate) fn take_voice_quit_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.extra
            .writes
            .take_quit_write(self.extra.network.take(), cx)
    }
}
