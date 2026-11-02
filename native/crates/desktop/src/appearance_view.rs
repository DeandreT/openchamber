//! Native Appearance settings backed by the window's retained appearance owner.

#[path = "settings_picker.rs"]
mod settings_picker;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AppContext, Context, Entity, EntityInputHandler, FocusHandle, FontWeight, Global,
    InteractiveElement, IntoElement, ParentElement, Render, Role, ScrollAnchor, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Subscription, TestSupportExt, Window, div,
    prelude::FluentBuilder, px,
};

use self::settings_picker::{OptionItem, Picker};
use crate::activation::on_activate;
use crate::appearance::{AppearancePreferences, AppearanceState, SaveStatus, ThemeMode};
use crate::appearance_storage::AppearanceError;
use crate::baseline::CssTextExt;
use crate::fonts;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::{self, Appearance, Palette};

pub(crate) struct AppearanceOwner(pub(crate) Entity<AppearanceState>);
impl Global for AppearanceOwner {}

#[derive(Clone, Copy)]
enum Field {
    Light,
    Dark,
    Locale,
    UiFont,
    MonoFont,
}

impl Field {
    fn index(self) -> usize {
        self as usize
    }
    fn id(self) -> &'static str {
        match self {
            Self::Light => "appearance-light-theme",
            Self::Dark => "appearance-dark-theme",
            Self::Locale => "appearance-locale",
            Self::UiFont => "appearance-ui-font",
            Self::MonoFont => "appearance-mono-font",
        }
    }
    fn item(self) -> &'static str {
        match self {
            Self::Light => "appearance.light-theme",
            Self::Dark => "appearance.dark-theme",
            Self::Locale => "appearance.language",
            Self::UiFont => "appearance.interface-font",
            Self::MonoFont => "appearance.code-font",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Light => "settings.openchamber.visual.field.lightTheme",
            Self::Dark => "settings.openchamber.visual.field.darkTheme",
            Self::Locale => "settings.appearance.language.label",
            Self::UiFont => "settings.openchamber.visual.field.interfaceFont",
            Self::MonoFont => "settings.openchamber.visual.field.codeFont",
        }
    }
    fn aria(self) -> &'static str {
        match self {
            Self::Light => "settings.openchamber.visual.field.selectLightThemeAria",
            Self::Dark => "settings.openchamber.visual.field.selectDarkThemeAria",
            Self::Locale => "settings.appearance.language.select",
            Self::UiFont => "settings.openchamber.visual.field.selectInterfaceFontAria",
            Self::MonoFont => "settings.openchamber.visual.field.selectCodeFontAria",
        }
    }
}

pub(crate) struct AppearanceView {
    owner: Option<Entity<AppearanceState>>,
    pickers: [Picker; 5],
    size: Entity<InputState>,
    size_invalid: bool,
    applied_size: u16,
    scroll: ScrollHandle,
    anchors: [ScrollAnchor; 6],
    highlight: Option<String>,
    mode_focus: [FocusHandle; 3],
    retry_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl AppearanceView {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.bind_keys([
            gpui_kit::KeyBinding::new(
                "ctrl-n",
                gpui_kit::base::actions::SelectDown,
                Some("SettingsPicker > List"),
            ),
            gpui_kit::KeyBinding::new(
                "ctrl-p",
                gpui_kit::base::actions::SelectUp,
                Some("SettingsPicker > List"),
            ),
        ]);
        let owner = cx
            .try_global::<AppearanceOwner>()
            .map(|owner| owner.0.clone());
        let percent = owner
            .as_ref()
            .map_or(100, |owner| owner.read(cx).preferences().font_size);
        let size = cx.new(|cx| InputState::new(window, cx).default_value(percent.to_string()));
        let mut subscriptions =
            vec![
                cx.subscribe_in(&size, window, |view, _, event, window, cx| {
                    let composing = view.size.update(cx, |input, cx| {
                        input.marked_text_range(window, cx).is_some()
                    });
                    if composing {
                        return;
                    }
                    match event {
                        InputEvent::Change => {
                            view.size_invalid = view
                                .size
                                .read(cx)
                                .value()
                                .parse::<u16>()
                                .map_or(true, |percent| !(50..=200).contains(&percent));
                            cx.notify();
                        }
                        InputEvent::PressEnter { .. } | InputEvent::Blur => {
                            view.commit_size(window, cx)
                        }
                        _ => {}
                    }
                }),
            ];
        if let Some(owner) = &owner {
            subscriptions.push(cx.observe_in(owner, window, |view, owner, window, cx| {
                let percent = owner.read(cx).preferences().font_size;
                if percent != view.applied_size {
                    let untouched =
                        view.size.read(cx).value().as_str() == view.applied_size.to_string();
                    let composing = view.size.update(cx, |input, cx| {
                        input.marked_text_range(window, cx).is_some()
                    });
                    view.applied_size = percent;
                    if untouched && !composing {
                        view.size.update(cx, |input, cx| {
                            input.set_value(percent.to_string(), window, cx)
                        });
                        view.size_invalid = false;
                    }
                }
                cx.notify();
            }));
        }
        let scroll = ScrollHandle::new();
        Self {
            owner,
            size,
            size_invalid: false,
            applied_size: percent,
            anchors: std::array::from_fn(|_| ScrollAnchor::for_handle(scroll.clone())),
            scroll,
            pickers: [
                Field::Light,
                Field::Dark,
                Field::Locale,
                Field::UiFont,
                Field::MonoFont,
            ]
            .map(|field| Picker::new(field, window, cx)),
            highlight: None,
            mode_focus: std::array::from_fn(|_| cx.focus_handle().tab_index(0).tab_stop(true)),
            retry_focus: cx.focus_handle().tab_index(0).tab_stop(true),
            _subscriptions: subscriptions,
        }
    }

    fn choose(&mut self, field: Field, value: &str, cx: &mut Context<Self>) {
        let Some(owner) = &self.owner else {
            return;
        };
        let _ = owner.update(cx, |state, cx| match field {
            Field::Light => state.set_light_theme(value, cx),
            Field::Dark => state.set_dark_theme(value, cx),
            Field::Locale => state.set_locale(value, cx),
            Field::UiFont => state.set_ui_font(value, cx),
            Field::MonoFont => state.set_mono_font(value, cx),
        });
    }

    fn choose_mode(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        if let Some(owner) = &self.owner {
            let _ = owner.update(cx, |state, cx| state.set_mode(mode, cx));
        }
    }

    /// Captures a committed, valid size draft without replacing the input text.
    pub(crate) fn commit_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.size.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        }) {
            return;
        }
        let percent = self
            .size
            .read(cx)
            .value()
            .parse::<u16>()
            .ok()
            .filter(|percent| (50..=200).contains(percent));
        self.size_invalid = percent.is_none();
        if let (Some(percent), Some(owner)) = (percent, &self.owner) {
            let _ = owner.update(cx, |state, cx| state.set_font_size(percent, cx));
        }
        // Invalid drafts stay intact. Enter or leaving the field saves only valid values.
        cx.notify();
    }

    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(owner) = &self.owner {
            if self.retry_focus.is_focused(window) {
                let mode = match owner.read(cx).preferences().mode {
                    ThemeMode::System => 0,
                    ThemeMode::Light => 1,
                    ThemeMode::Dark => 2,
                };
                window.focus(&self.mode_focus[mode], cx);
            }
            owner.update(cx, |state, cx| state.save(cx));
        }
    }

    fn status_slot(
        &self,
        error: Option<SharedString>,
        retryable: bool,
        cx: &Context<Self>,
    ) -> gpui_kit::AnyElement {
        let palette = cx.global::<Palette>();
        let hover = palette.hover;
        div()
            .id("appearance-error")
            .accessibility_id("appearance-error")
            .role(Role::Status)
            .aria_label(error.clone().unwrap_or_default())
            .h(gpui_kit::rems(2.5))
            .mb(px(16.0))
            .flex_row_from(locale::strings(cx).direction())
            .items_center()
            .gap(px(12.0))
            .when(error.is_some(), |row| row.bg(palette.error.opacity(0.08)))
            .children(error.map(|error| {
                div()
                    .id("appearance-error-message")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(palette.error)
                    .tooltip({
                        let error = error.clone();
                        move |window, cx| Tooltip::new(error.clone()).build(window, cx)
                    })
                    .child(error)
            }))
            .children(retryable.then(|| {
                on_activate(
                    div()
                        .id("appearance-retry")
                        .accessibility_id("appearance-retry")
                        .role(Role::Button)
                        .aria_label(locale::strings(cx).get("settings.appearance.actions.retry"))
                        .track_focus(&self.retry_focus)
                        .tab_index(0)
                        .h(gpui_kit::rems(2.0))
                        .px(px(12.0))
                        .cursor_pointer()
                        .hover(move |style| style.bg(hover))
                        .child(locale::strings(cx).get("settings.appearance.actions.retry")),
                    cx,
                    |view, window, cx| view.retry(window, cx),
                )
                .on_action(cx.listener(
                    |view, action: &gpui_kit::base::actions::Confirm, window, cx| {
                        if !action.secondary {
                            view.retry(window, cx);
                            cx.stop_propagation();
                        }
                    },
                ))
                .test_support()
            }))
            .into_any_element()
    }

    pub(crate) fn reveal(&mut self, item: &str, window: &mut Window, cx: &mut Context<Self>) {
        let index = match item {
            "appearance.light-theme" => 0,
            "appearance.dark-theme" => 1,
            "appearance.language" => 2,
            "appearance.interface-font" => 3,
            "appearance.code-font" => 4,
            "appearance.interface-font-size" => 5,
            _ => return,
        };
        self.highlight = Some(item.to_owned());
        cx.notify();
        // Wait until the newly selected Settings page has laid out its real rows.
        let entity = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = entity.update(cx, |view, cx| {
                view.anchors[index].scroll_to(window, cx);
                if index == 5 {
                    view.size.update(cx, |input, cx| input.focus(window, cx));
                } else {
                    view.pickers[index].focus(window, cx);
                }
                cx.notify();
            });
        });
    }

    fn field(
        &mut self,
        field: Field,
        preferences: &AppearancePreferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let strings = locale::strings(cx);
        let direction = strings.direction();
        let options: Vec<(String, SharedString)> = match field {
            Field::Light | Field::Dark => theme::options(if matches!(field, Field::Light) {
                Appearance::Light
            } else {
                Appearance::Dark
            })
            .into_iter()
            .map(|option| (option.id, option.name.into()))
            .collect(),
            Field::Locale => locale::supported_locales()
                .into_iter()
                .map(|code| {
                    let label = strings.locale_label(&code);
                    (code, label)
                })
                .collect(),
            Field::UiFont | Field::MonoFont => {
                let mono = matches!(field, Field::MonoFont);
                let fonts = if mono {
                    fonts::available_mono_fonts(cx)
                } else {
                    fonts::available_ui_fonts(cx)
                };
                let mut options = vec![(
                    if mono { "system-mono" } else { "system" }.to_owned(),
                    strings.get(if mono {
                        "settings.appearance.font.systemMono"
                    } else {
                        "settings.openchamber.visual.option.themeMode.system"
                    }),
                )];
                options.extend(
                    fonts
                        .into_iter()
                        .map(|font| (font.id.to_owned(), font.label.into())),
                );
                options
            }
        };
        let value = match field {
            Field::Light => &preferences.light_theme_id,
            Field::Dark => &preferences.dark_theme_id,
            Field::Locale => &preferences.locale,
            Field::UiFont => &preferences.ui_font,
            Field::MonoFont => &preferences.mono_font,
        };
        let label = strings.get(field.label());
        let options = options
            .into_iter()
            .map(|(id, label)| OptionItem {
                id: format!("{}-option-{id}", field.id()).into(),
                current: &id == value,
                value: id,
                label,
                field,
                owner: cx.weak_entity(),
            })
            .collect();
        let control = self.pickers[field.index()].control(field, options, value, window, cx);
        self.row(field.index(), field.item(), cx)
            .child(
                div()
                    .when(direction == Direction::RightToLeft, |label| {
                        label.text_right()
                    })
                    .child(label),
            )
            .child(div().flex_row_from(direction).child(control))
            .into_any_element()
    }

    fn row(
        &self,
        index: usize,
        item: &'static str,
        cx: &Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        div()
            .id(item)
            .accessibility_id(item)
            .role(Role::Group)
            .anchor_scroll(Some(self.anchors[index].clone()))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .min_w_0()
            .py(px(8.0))
            .when(self.highlight.as_deref() == Some(item), |row| {
                row.bg(cx.global::<Palette>().selection)
            })
    }

    fn heading(key: &'static str, divider: bool, cx: &Context<Self>) -> impl IntoElement + use<> {
        let strings = locale::strings(cx);
        div()
            .id(key)
            .role(Role::Heading)
            .css_text(1.0, 1.5, cx)
            .font_weight(FontWeight::SEMIBOLD)
            .when(divider, |title| {
                title
                    .mt(px(32.0))
                    .pt(px(32.0))
                    .border_t_1()
                    .border_color(cx.global::<Palette>().border)
            })
            .when(strings.direction() == Direction::RightToLeft, |title| {
                title.text_right()
            })
            .mb(px(16.0))
            .child(strings.get(key))
    }
}

impl Render for AppearanceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.owner.is_none() {
            let message = locale::strings(cx).get("common.unavailable");
            return div()
                .id("appearance-settings")
                .size_full()
                .overflow_y_scroll()
                .px(px(48.0))
                .py(px(32.0))
                .child(crate::settings_view::page_header(
                    crate::settings_navigation::Page::Appearance,
                    cx,
                ))
                .child(
                    div()
                        .id("appearance-error")
                        .accessibility_id("appearance-error")
                        .role(Role::Status)
                        .aria_label(message.clone())
                        .child(message),
                )
                .into_any_element();
        }
        let preferences = self
            .owner
            .as_ref()
            .map(|owner| owner.read(cx).preferences().clone())
            .unwrap_or_default();
        let status = self
            .owner
            .as_ref()
            .map(|owner| owner.read(cx).save_status());
        let palette = cx.global::<Palette>().clone();
        let direction = locale::strings(cx).direction();
        let mut radios = div()
            .id("appearance-color-mode")
            .flex()
            .flex_col()
            .gap(px(6.0))
            .role(Role::RadioGroup)
            .aria_label(locale::strings(cx).get("settings.openchamber.visual.section.colorMode"));
        for (index, (mode, name)) in [
            (ThemeMode::System, "system"),
            (ThemeMode::Light, "light"),
            (ThemeMode::Dark, "dark"),
        ]
        .into_iter()
        .enumerate()
        {
            let label = locale::strings(cx).get(&format!(
                "settings.openchamber.visual.option.themeMode.{name}"
            ));
            let selected = preferences.mode == mode;
            radios = radios.child(
                on_activate(
                    div()
                        .id(SharedString::from(format!("appearance-mode-{name}")))
                        .accessibility_id(format!("appearance-mode-{name}"))
                        .role(Role::RadioButton)
                        .aria_label(label.clone())
                        .aria_toggled(if selected {
                            gpui_kit::Toggled::True
                        } else {
                            gpui_kit::Toggled::False
                        })
                        .track_focus(&self.mode_focus[index])
                        .tab_index(0)
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(8.0))
                        .h(gpui_kit::rems(1.625))
                        .cursor_pointer()
                        .child(
                            div()
                                .size(gpui_kit::rems(1.0))
                                .rounded_full()
                                .border_1()
                                .border_color(palette.muted_foreground)
                                .flex()
                                .items_center()
                                .justify_center()
                                .children(selected.then(|| {
                                    div()
                                        .size(gpui_kit::rems(0.5))
                                        .rounded_full()
                                        .bg(palette.foreground)
                                })),
                        )
                        .child(label),
                    cx,
                    move |view, _, cx| view.choose_mode(mode, cx),
                )
                .on_action(cx.listener(
                    move |view, action: &gpui_kit::base::actions::Confirm, _, cx| {
                        if !action.secondary {
                            view.choose_mode(mode, cx);
                            cx.stop_propagation();
                        }
                    },
                ))
                .on_key_down(cx.listener(
                    move |view, event: &gpui_kit::KeyDownEvent, window, cx| {
                        let key = &event.keystroke;
                        if key.modifiers.control || key.modifiers.alt || key.modifiers.platform {
                            return;
                        }
                        let delta = match key.key.as_str() {
                            "down" => 1,
                            "up" => -1,
                            "right" => {
                                if direction == Direction::RightToLeft {
                                    -1
                                } else {
                                    1
                                }
                            }
                            "left" => {
                                if direction == Direction::RightToLeft {
                                    1
                                } else {
                                    -1
                                }
                            }
                            _ => return,
                        };
                        let next = (index as isize + delta).rem_euclid(3) as usize;
                        view.choose_mode(
                            [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark][next],
                            cx,
                        );
                        window.focus(&view.mode_focus[next], cx);
                        cx.stop_propagation();
                    },
                ))
                .test_support(),
            );
        }
        let light = self.field(Field::Light, &preferences, window, cx);
        let dark = self.field(Field::Dark, &preferences, window, cx);
        let language = self.field(Field::Locale, &preferences, window, cx);
        let ui = self.field(Field::UiFont, &preferences, window, cx);
        let mono = self.field(Field::MonoFont, &preferences, window, cx);
        // Flex bases wrap at the pane's 768px breakpoint, independent of the viewport.
        let pair = |left: gpui_kit::AnyElement, right: gpui_kit::AnyElement| {
            div()
                .flex_row_from(direction)
                .flex_wrap()
                .gap(px(32.0))
                .child(div().flex_1().flex_basis(px(368.0)).min_w_0().child(left))
                .child(div().flex_1().flex_basis(px(368.0)).min_w_0().child(right))
        };
        let error = if self.size_invalid {
            Some(locale::strings(cx).get("settings.appearance.native.error.fontSize"))
        } else {
            match status {
                Some(SaveStatus::Failed(error)) => Some(locale::strings(cx).get(match error {
                    AppearanceError::Unavailable => "settings.appearance.native.error.unavailable",
                    AppearanceError::Invalid => "settings.appearance.native.error.invalid",
                    AppearanceError::Changed => "settings.appearance.native.error.changed",
                    AppearanceError::Locked => "settings.appearance.native.error.locked",
                })),
                _ => None,
            }
        };
        let retryable = matches!(status, Some(SaveStatus::Failed(_)));
        let content = div()
            .max_w(px(840.0))
            .mx_auto()
            .text_left()
            .css_text(0.875, 1.25, cx)
            .child(crate::settings_view::page_header(
                crate::settings_navigation::Page::Appearance,
                cx,
            ))
            .child(self.status_slot(error, retryable, cx))
            .child(Self::heading(
                "settings.openchamber.visual.section.colorModeAndTheme",
                false,
                cx,
            ))
            .child(pair(
                radios.into_any_element(),
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(light)
                    .child(dark)
                    .into_any_element(),
            ))
            .child(Self::heading(
                "settings.openchamber.visual.section.localization",
                true,
                cx,
            ))
            .child(language)
            .child(Self::heading(
                "settings.openchamber.visual.section.densityAndType",
                true,
                cx,
            ))
            .child(pair(ui.into_any_element(), mono.into_any_element()))
            .child(
                self.row(5, "appearance.interface-font-size", cx)
                    .child(
                        div()
                            .when(direction == Direction::RightToLeft, |label| {
                                label.text_right()
                            })
                            .child(
                                locale::strings(cx)
                                    .get("settings.openchamber.visual.field.interfaceFontSize"),
                            ),
                    )
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(8.0))
                            .on_action(cx.listener(
                                |view, _: &gpui_kit::component::input::Escape, window, cx| {
                                    let composing = view.size.update(cx, |input, cx| {
                                        input.marked_text_range(window, cx).is_some()
                                    });
                                    if composing {
                                        return;
                                    }
                                    if let Some(owner) = &view.owner {
                                        let value =
                                            owner.read(cx).preferences().font_size.to_string();
                                        view.size.update(cx, |input, cx| {
                                            input.set_value(value, window, cx)
                                        });
                                        view.size_invalid = false;
                                        cx.notify();
                                        cx.stop_propagation();
                                    } else {
                                        cx.propagate();
                                    }
                                },
                            ))
                            .child(
                                Input::new(&self.size)
                                    .focus_bordered(false)
                                    .id("appearance-font-size")
                                    .accessibility_id("appearance-font-size")
                                    .aria_label(locale::strings(cx).get(
                                        "settings.openchamber.visual.field.fontSizePercentageAria",
                                    ))
                                    .w(px(80.0))
                                    .h(gpui_kit::rems(2.0)),
                            )
                            .child("%"),
                    ),
            );
        div()
            .id("appearance-settings")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(48.0))
            .py(px(32.0))
            .child(content)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::appearance_storage::AppearanceStore;

    fn conflict_recovery(cx: &mut gpui_kit::TestAppContext, draft: Option<&str>) {
        cx.executor().allow_parking();
        let fixture = crate::recovery_tests::Fixture::new();
        let journal = fixture.journal();
        let store = AppearanceStore::new(journal.parent().expect("fixture profile directory"));
        let initial = store
            .save(None, &AppearancePreferences::default())
            .expect("initial preferences");
        let (window, _) = crate::workspace_tests::open(cx);
        let (owner, view) = cx
            .update_window(window, |_, window, cx| {
                let owner = cx.new(|cx| {
                    AppearanceState::new(
                        store.clone(),
                        store.load(),
                        fonts::GenericFamilies::default(),
                        cx,
                    )
                });
                cx.set_global(AppearanceOwner(owner.clone()));
                let view = cx.new(|cx| AppearanceView::new(window, cx));
                if let Some(draft) = draft {
                    view.update(cx, |view, cx| {
                        view.size.update(cx, |input, cx| {
                            input.set_value(draft.to_owned(), window, cx)
                        })
                    });
                }
                (owner, view)
            })
            .expect("test window");
        cx.run_until_parked();
        let foreign = AppearancePreferences {
            font_size: 125,
            ..initial.preferences.clone()
        };
        store
            .save(Some(&initial), &foreign)
            .expect("foreign font-size edit");
        owner.update(cx, |state, cx| {
            let locale = if state.preferences().locale == "ar" {
                "en"
            } else {
                "ar"
            };
            state
                .set_locale(locale, cx)
                .expect("independent local edit")
        });
        cx.run_until_parked();
        owner.update(cx, |state, cx| {
            assert_eq!(
                state.save_status(),
                SaveStatus::Failed(AppearanceError::Changed),
                "foreign edit causes an actual persistence conflict"
            );
            state.save(cx);
        });
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(
                owner.read(cx).preferences().font_size,
                125,
                "recovery merges the foreign font size"
            );
            assert_eq!(
                view.read(cx).size.read(cx).value().as_str(),
                draft.unwrap_or("125"),
                "recovery updates untouched input and preserves uncommitted drafts"
            );
        });
        if draft.is_none() {
            view.update(cx, |view, cx| {
                view.size.update(cx, |_, cx| cx.emit(InputEvent::Blur))
            });
            cx.run_until_parked();
            cx.read(|cx| {
                assert_eq!(
                    owner.read(cx).preferences().font_size,
                    125,
                    "untouched blur cannot overwrite a recovered preference"
                )
            });
            assert_eq!(
                store
                    .load()
                    .expect("saved preferences")
                    .expect("snapshot")
                    .preferences
                    .font_size,
                125,
                "untouched blur preserves the recovered disk value"
            );
        }
    }

    #[gpui_kit::test]
    fn recovered_size_survives_untouched_blur(cx: &mut gpui_kit::TestAppContext) {
        conflict_recovery(cx, None);
    }

    #[gpui_kit::test]
    fn recovery_preserves_edited_size_draft(cx: &mut gpui_kit::TestAppContext) {
        conflict_recovery(cx, Some("137"));
    }

    #[gpui_kit::test]
    fn recovery_preserves_invalid_size_draft(cx: &mut gpui_kit::TestAppContext) {
        conflict_recovery(cx, Some("invalid"));
    }

    enum Composition {
        Marked,
        Committed,
    }

    enum Close {
        Window,
        Application,
    }

    fn prepare_close_size(
        cx: &mut gpui_kit::TestAppContext,
        draft: &str,
        composition: Composition,
        close: Close,
        expected: u16,
    ) {
        cx.executor().allow_parking();
        let fixture = crate::recovery_tests::Fixture::new();
        let journal = fixture.journal();
        let store = AppearanceStore::new(journal.parent().expect("fixture profile directory"));
        store
            .save(None, &AppearancePreferences::default())
            .expect("initial preferences");
        let (window, workspace) = crate::workspace_tests::open(cx);
        let created = Rc::new(RefCell::new(None));
        let (owner, subscription) = cx.update(|cx| {
            let owner = cx.new(|cx| {
                AppearanceState::new(
                    store.clone(),
                    store.load(),
                    fonts::GenericFamilies::default(),
                    cx,
                )
            });
            cx.set_global(AppearanceOwner(owner.clone()));
            let created = created.clone();
            let subscription = cx.observe_new::<AppearanceView>(move |_, _, cx| {
                *created.borrow_mut() = Some(cx.entity());
            });
            (owner, subscription)
        });
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.open_settings(window, cx));
        })
        .expect("test window");
        let view = created
            .borrow_mut()
            .take()
            .expect("Settings creates its retained Appearance view");
        drop(subscription);
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.size.update(cx, |input, cx| {
                    input.replace_and_mark_text_in_range(Some(0..3), draft, None, window, cx);
                    assert!(
                        input.marked_text_range(window, cx).is_some(),
                        "the platform input handler establishes an actual preedit"
                    );
                    if matches!(composition, Composition::Committed) {
                        input.replace_text_in_range(None, draft, window, cx);
                        assert!(
                            input.marked_text_range(window, cx).is_none(),
                            "the platform text commit clears the actual preedit"
                        );
                    }
                });
            });
        })
        .expect("test window");
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(
                owner.read(cx).preferences().font_size,
                100,
                "typing alone leaves the field pending without Enter or Blur"
            );
        });
        match close {
            Close::Window => {
                cx.update_window(window, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| workspace.request_close(window, cx));
                })
                .expect("normal close starts in the owning window");
                cx.run_until_parked();
            }
            Close::Application => cx.quit(),
        }
        assert!(
            window.update(cx, |_, _, _| ()).is_err(),
            "the window is removed after close preparation and persistence"
        );
        cx.read(|cx| {
            assert_eq!(
                owner.read(cx).preferences().font_size,
                expected,
                "close captures only valid, committed pending input"
            );
            assert_eq!(
                view.read(cx).size.read(cx).value().as_str(),
                draft,
                "close preparation does not rewrite the retained draft"
            );
        });
        assert_eq!(
            store
                .load()
                .expect("saved preferences")
                .expect("snapshot")
                .preferences
                .font_size,
            expected,
            "the close barrier makes the accepted value durable"
        );
    }

    #[gpui_kit::test]
    fn prepare_close_captures_valid_size_without_enter_or_blur(cx: &mut gpui_kit::TestAppContext) {
        prepare_close_size(cx, "137", Composition::Committed, Close::Window, 137);
    }

    #[gpui_kit::test]
    fn prepare_close_does_not_commit_invalid_size(cx: &mut gpui_kit::TestAppContext) {
        prepare_close_size(cx, "invalid", Composition::Committed, Close::Window, 100);
    }

    #[gpui_kit::test]
    fn prepare_close_does_not_commit_numeric_preedit(cx: &mut gpui_kit::TestAppContext) {
        prepare_close_size(cx, "137", Composition::Marked, Close::Window, 100);
    }

    #[gpui_kit::test]
    fn prepare_close_on_app_quit_captures_committed_size(cx: &mut gpui_kit::TestAppContext) {
        prepare_close_size(cx, "137", Composition::Committed, Close::Application, 137);
    }

    #[gpui_kit::test]
    fn prepare_close_on_app_quit_does_not_commit_numeric_preedit(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        prepare_close_size(cx, "137", Composition::Marked, Close::Application, 100);
    }
}
