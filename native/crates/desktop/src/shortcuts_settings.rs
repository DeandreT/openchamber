//! The Shortcuts settings page (`KeyboardShortcutsSettings.tsx`) and its
//! recording dialog (`ShortcutRecordingDialog.tsx`).
//!
//! The page lists every customizable in-app action by category with its
//! binding, Edit and, for a changed binding, Reset. The recorder keeps the
//! keys local until Confirm; Escape, Enter and Tab are recordable keys, so
//! only its buttons apply or discard a recording.

use std::time::Duration;

use gpui_kit::component::{DialogLayerHandle, WindowExt};
use gpui_kit::{
    AppContext, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement, KeyBinding,
    KeyDownEvent, Modifiers, ModifiersChangedEvent, ParentElement, Render, Role, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Task, WeakEntity, Window, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::shortcuts::{
    self, Category, KeyPhase, Overrides, Platform, RecordedKey, Recording, RecordingVerdict,
    ShortcutAction,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::CONTROL_RADIUS;
use crate::locale::{self, Strings};
use crate::logical::LogicalStyledExt;
use crate::shortcuts::ShortcutBindings;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The recorder's key context: the keys a dialog or the window would take
/// are recorded instead.
const RECORDER_CONTEXT: &str = "ShortcutRecorder";

pub(crate) fn bind_keys(cx: &mut gpui_kit::App) {
    cx.bind_keys(
        ["escape", "enter", "tab", "shift-tab", "space"]
            .into_iter()
            .map(|keys| KeyBinding::new(keys, gpui_kit::NoAction, Some(RECORDER_CONTEXT))),
    );
}

fn overrides(cx: &gpui_kit::App) -> Overrides {
    cx.try_global::<ShortcutBindings>()
        .map(|bindings| bindings.overrides.clone())
        .unwrap_or_default()
}

/// An action's label: its Settings label, or its fixed binding for an
/// action users cannot rebind.
fn action_label(action: &ShortcutAction, strings: &Strings) -> SharedString {
    match action.label_key {
        Some(key) => strings.get(key),
        None => {
            shortcuts::format_for_display(action.default_binding, "", Platform::current()).into()
        }
    }
}

/// What a row shows: the binding, plus the digit suffix of a prefix.
fn binding_text(action: &ShortcutAction, overrides: &Overrides, strings: &Strings) -> String {
    let unassigned = strings.get("settings.openchamber.keyboardShortcuts.unassigned");
    if !action.prefix_style {
        return shortcuts::format_for_display(
            &shortcuts::effective_combo(action.id, overrides),
            &unassigned,
            Platform::current(),
        );
    }
    let combo = shortcuts::effective_prefix(action.id, overrides);
    let formatted = shortcuts::format_for_display(&combo, &unassigned, Platform::current());
    if combo.is_empty() || combo == shortcuts::UNASSIGNED {
        return formatted;
    }
    let suffix = if action.id == "switch_session_tab" {
        "settings.openchamber.keyboardShortcuts.action.switch_session_tab.suffix"
    } else {
        "settings.openchamber.keyboardShortcuts.action.switch_context_surface.suffix"
    };
    format!("{formatted}{}", strings.get(suffix))
}

pub(crate) struct ShortcutsPage {
    owner: WeakEntity<Workspace>,
    scroll: ScrollHandle,
}

impl ShortcutsPage {
    pub(crate) fn new(owner: WeakEntity<Workspace>) -> Self {
        Self {
            owner,
            scroll: ScrollHandle::new(),
        }
    }

    fn save(&self, next: Overrides, cx: &mut Context<Self>) {
        let _ = self.owner.update(cx, |workspace, cx| {
            workspace.save_shortcut_overrides(next, cx);
        });
        cx.notify();
    }

    fn edit(
        &mut self,
        action: &'static ShortcutAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = cx.weak_entity();
        let recorder = cx.new(|cx| Recorder::new(action, page, cx));
        let handle = window.open_dialog_with_handle(cx, {
            let recorder = recorder.clone();
            move |dialog, _, _| {
                dialog
                    .w(px(448.0))
                    .close_button(false)
                    .keyboard(false)
                    .overlay_closable(false)
                    .child(recorder.clone())
            }
        });
        recorder.update(cx, |recorder, cx| {
            recorder.handle = Some(handle);
            recorder.focus.focus(window, cx);
        });
    }
}

impl Render for ShortcutsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let palette = cx.global::<Palette>().clone();
        let current = overrides(cx);
        let failed = cx
            .try_global::<ShortcutBindings>()
            .is_some_and(|bindings| bindings.failed);
        let small_button = |id: SharedString, label: SharedString, outline: bool| {
            gpui_kit::base::Button::new(id.clone())
                .accessibility_id(id)
                .accessibility_label(label.clone())
                .h(px(24.0))
                .px(px(8.0))
                .rounded(px(CONTROL_RADIUS))
                .when(outline, |this| this.border_1().border_color(palette.border))
                .when(!outline, |this| this.bg(palette.muted))
                .hover(|style| style.bg(palette.hover))
                .css_text(0.75, 1.0, cx)
                .child(label)
        };
        let mut page = div()
            .id("shortcuts-settings-page")
            .role(Role::Group)
            .aria_label(strings.get("settings.page.shortcuts.title"))
            .size_full()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(48.0))
            .py(px(32.0))
            .css_text(0.875, 1.25, cx)
            .text_color(palette.foreground)
            .when(direction == locale::Direction::RightToLeft, |view| {
                view.text_right()
            })
            .child(crate::settings_view::page_header(
                crate::settings_navigation::Page::Shortcuts,
                cx,
            ))
            .when(failed, |page| {
                page.child(
                    div()
                        .id("shortcuts-save-failed")
                        .role(Role::Status)
                        .mb(px(16.0))
                        .text_color(palette.error)
                        .child(strings.get("settings.common.status.saveFailed")),
                )
            });
        for (index, category) in Category::LISTED.into_iter().enumerate() {
            let actions: Vec<&'static ShortcutAction> = shortcuts::SCHEMA
                .iter()
                .filter(|action| action.category == category && action.customizable())
                .collect();
            let mut section = div()
                .id(SharedString::from(format!("shortcuts-section-{index}")))
                .when(index > 0, |section| {
                    section
                        .mt(px(32.0))
                        .pt(px(24.0))
                        .border_t_1()
                        .border_color(palette.border)
                })
                .child(
                    div()
                        .flex_row_from(direction)
                        .items_center()
                        .justify_between()
                        .mb(px(12.0))
                        .child(
                            div()
                                .id(SharedString::from(category.title_key()))
                                .role(Role::Heading)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(strings.get(category.title_key())),
                        )
                        .when(index == 0, |header| {
                            header.child(on_activate(
                                small_button(
                                    "shortcuts-reset-all".into(),
                                    strings.get(
                                        "settings.openchamber.keyboardShortcuts.actions.resetAll",
                                    ),
                                    true,
                                ),
                                cx,
                                |this: &mut Self, _, cx| this.save(Overrides::new(), cx),
                            ))
                        }),
                );
            for action in actions {
                let label = strings.get(action.label_key.expect("listed actions are customizable"));
                let overridden = current.contains_key(action.id);
                section = section.child(
                    div()
                        .id(SharedString::from(format!("shortcut-row-{}", action.id)))
                        .accessibility_id(format!("shortcut-row-{}", action.id))
                        .min_h(px(36.0))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(8.0))
                        .child(div().flex_1().min_w_0().child(label.clone()))
                        .child(
                            div()
                                .id(SharedString::from(format!(
                                    "shortcut-binding-{}",
                                    action.id
                                )))
                                .accessibility_id(format!("shortcut-binding-{}", action.id))
                                .role(Role::Label)
                                .aria_label(binding_text(action, &current, &strings))
                                .min_w(px(128.0))
                                .px(px(8.0))
                                .py(px(4.0))
                                .rounded(px(CONTROL_RADIUS))
                                .bg(palette.muted)
                                .text_center()
                                .css_text(0.75, 1.0, cx)
                                .child(binding_text(action, &current, &strings)),
                        )
                        .child(on_activate(
                            small_button(
                                format!("shortcut-edit-{}", action.id).into(),
                                strings.get("settings.openchamber.keyboardShortcuts.actions.edit"),
                                false,
                            ),
                            cx,
                            move |this: &mut Self, window, cx| this.edit(action, window, cx),
                        ))
                        .when(overridden, |row| {
                            row.child(on_activate(
                                small_button(
                                    format!("shortcut-reset-{}", action.id).into(),
                                    strings.get("settings.common.actions.reset"),
                                    true,
                                ),
                                cx,
                                move |this: &mut Self, _, cx| {
                                    let mut next = overrides(cx);
                                    next.remove(action.id);
                                    this.save(next, cx);
                                },
                            ))
                        }),
                );
            }
            page = page.child(section);
        }
        page
    }
}

/// The recording dialog for one action.
pub(crate) struct Recorder {
    action: &'static ShortcutAction,
    recording: Recording,
    held: Modifiers,
    page: WeakEntity<ShortcutsPage>,
    focus: FocusHandle,
    handle: Option<DialogLayerHandle>,
    second_chord: Option<Task<()>>,
}

fn recorded(key: &str, modifiers: Modifiers) -> RecordedKey {
    let key = match key {
        "up" => "arrowup".to_owned(),
        "down" => "arrowdown".to_owned(),
        "left" => "arrowleft".to_owned(),
        "right" => "arrowright".to_owned(),
        other => shortcuts::key_token(other),
    };
    RecordedKey {
        key,
        control: modifiers.control,
        meta: modifiers.platform,
        shift: modifiers.shift,
        alt: modifiers.alt,
        repeat: false,
        composing: false,
    }
}

impl Recorder {
    fn new(
        action: &'static ShortcutAction,
        page: WeakEntity<ShortcutsPage>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            action,
            recording: Recording::default(),
            held: Modifiers::default(),
            page,
            focus: cx.focus_handle().tab_index(0).tab_stop(true),
            handle: None,
            second_chord: None,
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let mut key = recorded(&event.keystroke.key, event.keystroke.modifiers);
        key.repeat = event.is_held;
        let before = self.recording.clone();
        self.recording
            .apply(&key, KeyPhase::Down, 2, Platform::current());
        if self.action.prefix_style && self.recording.chords.len() > 1 {
            self.recording = before;
        }
        self.arm_second_chord(cx);
        cx.notify();
    }

    fn modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = event.modifiers;
        let previous = std::mem::replace(&mut self.held, now);
        let released = [
            (previous.control && !now.control, "control"),
            (previous.alt && !now.alt, "alt"),
            (previous.shift && !now.shift, "shift"),
            (previous.platform && !now.platform, "meta"),
        ]
        .into_iter()
        .find_map(|(released, name)| released.then_some(name));
        let platform = Platform::current();
        match released {
            Some(name) => {
                let key = RecordedKey {
                    key: name.to_owned(),
                    ..recorded("", now)
                };
                if !(self.action.prefix_style
                    && self.recording.apply_prefix_release(&key, platform))
                {
                    self.recording.apply(&key, KeyPhase::Up, 2, platform);
                }
            }
            None => {
                let key = RecordedKey {
                    key: "control".to_owned(),
                    ..recorded("", now)
                };
                self.recording.apply(&key, KeyPhase::Down, 2, platform);
            }
        }
        cx.notify();
    }

    /// After the first chord the recorder waits up to three seconds for a
    /// second one.
    fn arm_second_chord(&mut self, cx: &mut Context<Self>) {
        if self.recording.chords.len() != 1 || self.recording.settled {
            self.second_chord = None;
            return;
        }
        self.second_chord = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(shortcuts::SECOND_CHORD_TIMEOUT_MS))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.recording.settle();
                cx.notify();
            });
        }));
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = self.handle.take() {
            window.close_dialog_by_handle(&handle, cx);
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.recording.settle();
        self.second_chord = None;
        let current = overrides(cx);
        let combo = self.recording.combo();
        let verdict = shortcuts::recording_verdict(self.action.id, &combo, &current);
        if !verdict.can_save() {
            cx.notify();
            return;
        }
        let replaced = match verdict {
            RecordingVerdict::Replaces(previous) => Some(previous.id),
            _ => None,
        };
        let next = shortcuts::save_override(&current, self.action.id, &combo, replaced);
        let _ = self.page.update(cx, |page, cx| page.save(next, cx));
        self.close(window, cx);
    }
}

impl Render for Recorder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let palette = cx.global::<Palette>().clone();
        let platform = Platform::current();
        let combo = self.recording.combo();
        let verdict = shortcuts::recording_verdict(self.action.id, &combo, &overrides(cx));
        let settled = self.recording.settled;
        let blocked = settled
            && matches!(
                verdict,
                RecordingVerdict::Internal | RecordingVerdict::Prefix(_)
            );
        let message = |text: SharedString, error: bool| {
            div()
                .id("shortcut-recorder-message")
                .role(Role::Status)
                .css_text(0.75, 1.0, cx)
                .text_color(if error {
                    palette.error
                } else {
                    palette.warning
                })
                .child(text)
        };
        let key = |text: String, live: bool| {
            div()
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(CONTROL_RADIUS))
                .border_1()
                .border_color(palette.border)
                .when(live, |key| {
                    key.border_dashed().text_color(palette.muted_foreground)
                })
                .bg(palette.muted)
                .child(text)
        };
        let button = |id: &'static str, label: SharedString, primary: bool, disabled: bool| {
            gpui_kit::base::Button::new(id)
                .accessibility_id(id)
                .accessibility_label(label.clone())
                .disabled(disabled)
                .h(px(28.0))
                .px(px(12.0))
                .rounded(px(CONTROL_RADIUS))
                .when(primary, |this| {
                    this.bg(palette.primary)
                        .text_color(palette.primary_foreground)
                })
                .when(!primary, |this| this.hover(|style| style.bg(palette.hover)))
                .when(disabled, |this| this.opacity(0.5))
                .child(label)
        };
        let mut keys = div()
            .flex_row_from(direction)
            .flex_wrap()
            .items_center()
            .justify_center()
            .gap(px(8.0));
        for chord in &self.recording.chords {
            keys = keys.child(key(
                shortcuts::format_for_display(chord, "", platform),
                false,
            ));
        }
        if let Some(preview) = &self.recording.live_preview {
            keys = keys.child(key(
                shortcuts::format_for_display(preview, "", platform),
                true,
            ));
        }
        if self.recording.chords.is_empty() && self.recording.live_preview.is_none() {
            keys = keys.child(
                div()
                    .text_color(palette.muted_foreground)
                    .child(strings.get("settings.openchamber.keyboardShortcuts.dialog.recording")),
            );
        }
        div()
            .id("shortcut-recorder")
            .accessibility_id("shortcut-recorder")
            .role(Role::Group)
            .flex()
            .flex_col()
            .gap(px(12.0))
            .css_text(0.875, 1.25, cx)
            .text_color(palette.foreground)
            .child(
                div()
                    .id("shortcut-recorder-title")
                    .role(Role::Heading)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(strings.format(
                        "settings.openchamber.keyboardShortcuts.dialog.title",
                        &[("action", &action_label(self.action, &strings))],
                    )),
            )
            .child(
                div()
                    .text_color(palette.muted_foreground)
                    .css_text(0.8125, 1.25, cx)
                    .child(
                        strings.get("settings.openchamber.keyboardShortcuts.dialog.instructions"),
                    ),
            )
            .child(
                div()
                    .id("shortcut-recorder-keys")
                    .accessibility_id("shortcut-recorder-keys")
                    .key_context(RECORDER_CONTEXT)
                    .track_focus(&self.focus)
                    .min_h(px(112.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(px(16.0))
                    .py(px(20.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(palette.border)
                    .bg(palette.elevated)
                    .on_key_down(cx.listener(Self::key_down))
                    .on_modifiers_changed(cx.listener(Self::modifiers_changed))
                    .child(keys),
            )
            .when(settled, |this| match verdict {
                RecordingVerdict::Internal => this.child(message(
                    strings.get("settings.openchamber.keyboardShortcuts.error.internalConflict"),
                    true,
                )),
                RecordingVerdict::Prefix(other) => this.child(message(
                    strings.format(
                        "settings.openchamber.keyboardShortcuts.error.prefixConflict",
                        &[("action", &action_label(other, &strings))],
                    ),
                    true,
                )),
                RecordingVerdict::Replaces(other) => this.child(message(
                    strings.format(
                        "settings.openchamber.keyboardShortcuts.error.exactConflict",
                        &[("action", &action_label(other, &strings))],
                    ),
                    false,
                )),
                RecordingVerdict::Empty | RecordingVerdict::Saves => this,
            })
            .when(
                settled && shortcuts::is_risky_browser_shortcut(&combo),
                |this| {
                    this.child(message(
                        strings.get(
                            "settings.openchamber.keyboardShortcuts.warning.riskyBrowserShortcut",
                        ),
                        false,
                    ))
                },
            )
            .child(
                div()
                    .flex_row_from(direction)
                    .justify_end()
                    .gap(px(8.0))
                    .child(on_activate(
                        button(
                            "shortcut-recorder-cancel",
                            strings.get("settings.common.actions.cancel"),
                            false,
                            false,
                        ),
                        cx,
                        |this: &mut Self, window, cx| this.close(window, cx),
                    ))
                    .child(on_activate(
                        button(
                            "shortcut-recorder-confirm",
                            strings.get("settings.openchamber.keyboardShortcuts.actions.confirm"),
                            true,
                            combo.is_empty() || blocked,
                        ),
                        cx,
                        |this: &mut Self, window, cx| this.confirm(window, cx),
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_show_bindings_and_prefix_suffixes() {
        let strings = Strings::load("en");
        let sidebar = shortcuts::action("toggle_sidebar").expect("declared");
        let empty = Overrides::new();
        assert_eq!(binding_text(sidebar, &empty, &strings), "Ctrl + B");
        let removed =
            shortcuts::save_override(&empty, "toggle_sidebar", shortcuts::UNASSIGNED, None);
        assert_eq!(
            binding_text(sidebar, &removed, &strings),
            &*strings.get("settings.openchamber.keyboardShortcuts.unassigned")
        );
        let tabs = shortcuts::action("switch_session_tab").expect("declared");
        assert_eq!(
            binding_text(tabs, &empty, &strings),
            format!(
                "Ctrl{}",
                strings
                    .get("settings.openchamber.keyboardShortcuts.action.switch_session_tab.suffix")
            )
        );
    }

    #[test]
    fn gpui_keys_record_as_reference_tokens() {
        let modifiers = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        assert_eq!(recorded("up", modifiers).key, "arrowup");
        assert_eq!(recorded(",", modifiers).key, "comma");
        assert!(recorded("k", modifiers).control);
        let mut recording = Recording::default();
        recording.apply(
            &recorded("k", modifiers),
            KeyPhase::Down,
            2,
            Platform::Other,
        );
        recording.apply(
            &recorded("escape", Modifiers::default()),
            KeyPhase::Down,
            2,
            Platform::Other,
        );
        assert_eq!(recording.combo(), "mod+k escape");
    }
}
