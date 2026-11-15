//! Native Appearance settings backed by the window's retained appearance owner.

#[path = "appearance_catalog.rs"]
mod catalog;
#[path = "appearance_schematic.rs"]
mod schematic;
#[path = "settings_picker.rs"]
mod settings_picker;

use std::collections::HashSet;
use std::sync::Arc;

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AppContext, Context, Entity, EntityInputHandler, FocusHandle, FontWeight, Global, Image,
    InteractiveElement, IntoElement, ParentElement, Render, Role, ScrollAnchor, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Subscription, TestSupportExt, Window, div,
    img, prelude::FluentBuilder, px,
};

use self::settings_picker::{OptionItem, Picker};
use crate::activation::on_activate;
use crate::appearance::{AppearancePreferences, AppearanceState, SaveStatus, ThemeMode};
use crate::appearance_storage::AppearanceError;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::elevation;
use crate::fonts;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::{self, Appearance, Palette};

pub(crate) struct AppearanceOwner(pub(crate) Entity<AppearanceState>);
impl Global for AppearanceOwner {}

#[derive(Clone, Copy)]
enum Field {
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
            Self::Locale => "appearance-locale",
            Self::UiFont => "appearance-ui-font",
            Self::MonoFont => "appearance-mono-font",
        }
    }
    fn item(self) -> &'static str {
        match self {
            Self::Locale => "appearance.language",
            Self::UiFont => "appearance.interface-font",
            Self::MonoFont => "appearance.code-font",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Locale => "settings.appearance.language.label",
            Self::UiFont => "settings.openchamber.visual.field.interfaceFont",
            Self::MonoFont => "settings.openchamber.visual.field.codeFont",
        }
    }
    fn aria(self) -> &'static str {
        match self {
            Self::Locale => "settings.appearance.language.select",
            Self::UiFont => "settings.openchamber.visual.field.selectInterfaceFontAria",
            Self::MonoFont => "settings.openchamber.visual.field.selectCodeFontAria",
        }
    }
}

type ThemeSchematics = ((String, String), [Arc<Image>; 3]);

pub(crate) struct AppearanceView {
    owner: Option<Entity<AppearanceState>>,
    pickers: [Picker; 3],
    themes: Vec<ThemeRow>,
    theme_revision: Option<u64>,
    deleting: HashSet<String>,
    import_focus: FocusHandle,
    theme_search: Entity<InputState>,
    variant: Option<Appearance>,
    preview: Option<String>,
    theme_focus_id: Option<String>,
    theme_scroll: ScrollHandle,
    theme_list_focus: FocusHandle,
    variant_focus: [FocusHandle; 2],
    schematics: Option<ThemeSchematics>,
    size: Entity<InputState>,
    size_invalid: bool,
    applied_size: u16,
    scroll: ScrollHandle,
    anchors: [ScrollAnchor; 8],
    highlight: Option<String>,
    mode_focus: [FocusHandle; 3],
    retry_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

struct ThemeRow {
    theme: theme::ThemeOption,
    focus: FocusHandle,
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
        let theme_search = cx.new(|cx| InputState::new(window, cx));
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
        subscriptions.push(cx.subscribe(&theme_search, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        }));
        let theme_list_focus = cx.focus_handle();
        subscriptions.push(
            cx.on_focus_out(&theme_list_focus, window, |view, _, _, cx| {
                view.preview = None;
                cx.notify();
            }),
        );
        if let Some(owner) = &owner {
            subscriptions.push(cx.observe_in(owner, window, |view, owner, window, cx| {
                let percent = owner.read(cx).preferences().font_size;
                view.sync_themes(window, cx);
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
            theme_revision: None,
            deleting: HashSet::new(),
            import_focus: cx.focus_handle().tab_stop(true),
            themes: [Appearance::Light, Appearance::Dark]
                .into_iter()
                .flat_map(theme::options)
                .map(|theme| ThemeRow {
                    theme,
                    focus: cx.focus_handle().tab_stop(true),
                })
                .collect(),
            theme_search,
            variant: None,
            preview: None,
            theme_focus_id: None,
            theme_scroll: ScrollHandle::new(),
            theme_list_focus,
            variant_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            schematics: None,
            size,
            size_invalid: false,
            applied_size: percent,
            anchors: std::array::from_fn(|_| ScrollAnchor::for_handle(scroll.clone())),
            scroll,
            pickers: [Field::Locale, Field::UiFont, Field::MonoFont]
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
            Field::Locale => state.set_locale(value, cx),
            Field::UiFont => state.set_ui_font(value, cx),
            Field::MonoFont => state.set_mono_font(value, cx),
        });
    }

    fn choose_mode(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        match mode {
            ThemeMode::Light => self.select_variant(Appearance::Light, cx),
            ThemeMode::Dark => self.select_variant(Appearance::Dark, cx),
            ThemeMode::System => {}
        }
        if let Some(owner) = &self.owner {
            let _ = owner.update(cx, |state, cx| state.set_mode(mode, cx));
        }
    }

    fn variant(&self, cx: &Context<Self>) -> Appearance {
        self.variant.unwrap_or(cx.global::<Palette>().appearance)
    }

    fn select_variant(&mut self, variant: Appearance, cx: &mut Context<Self>) {
        self.variant = Some(variant);
        self.preview = None;
        self.theme_focus_id = None;
        self.theme_scroll
            .set_offset(gpui_kit::point(px(0.0), px(0.0)));
        if let Some(owner) = &self.owner {
            let preferences = owner.read(cx).preferences();
            let id = selected_theme(preferences, variant);
            if let Some(index) = self
                .filtered_themes(cx)
                .iter()
                .position(|i| self.themes[*i].theme.id == id)
            {
                self.theme_scroll.scroll_to_item(index);
            }
        }
        cx.notify();
    }

    fn select_theme(&mut self, index: usize, cx: &mut Context<Self>) {
        let theme = &self.themes[index].theme;
        if let Some(owner) = &self.owner {
            let _ = owner.update(cx, |state, cx| match theme.palette.appearance {
                Appearance::Light => state.set_light_theme(&theme.id, cx),
                Appearance::Dark => state.set_dark_theme(&theme.id, cx),
            });
        }
        self.preview = None;
        cx.notify();
    }

    fn sync_themes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(owner) = &self.owner else {
            return;
        };
        let state = owner.read(cx);
        let revision = state.theme_revision();
        if self.theme_revision == Some(revision) {
            return;
        }
        let themes = state.available_themes();
        let old_rows = self.filtered_themes(cx);
        let focused_position = old_rows
            .iter()
            .position(|index| self.themes[*index].focus.contains_focused(window, cx));
        let mut old = std::mem::take(&mut self.themes);
        self.themes = themes
            .into_iter()
            .map(|theme| {
                let focus = old
                    .iter()
                    .position(|row| row.theme.id == theme.id)
                    .map(|index| old.swap_remove(index).focus)
                    .unwrap_or_else(|| cx.focus_handle().tab_stop(true));
                ThemeRow { theme, focus }
            })
            .collect();
        self.theme_revision = Some(revision);
        self.schematics = None;
        if self
            .preview
            .as_ref()
            .is_some_and(|id| !self.themes.iter().any(|row| &row.theme.id == id))
        {
            self.preview = None;
        }
        if old.iter().any(|row| row.focus.contains_focused(window, cx)) {
            let rows = self.filtered_themes(cx);
            if let Some(index) = rows.get(
                focused_position
                    .unwrap_or(0)
                    .min(rows.len().saturating_sub(1)),
            ) {
                self.theme_focus_id = Some(self.themes[*index].theme.id.clone());
                window.focus(&self.themes[*index].focus, cx);
            } else {
                window.focus(
                    &self.variant_focus[usize::from(self.variant(cx) == Appearance::Dark)],
                    cx,
                );
            }
        }
    }

    fn delete_theme(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(owner) = &self.owner else {
            return;
        };
        let id = self.themes[index].theme.id.clone();
        if !owner.read(cx).is_custom_theme(&id) || !self.deleting.insert(id.clone()) {
            return;
        }
        let result = owner.update(cx, |state, cx| state.delete_theme(&id, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = result.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.deleting.remove(&id);
                if !matches!(result, Ok(Ok(()))) {
                    let text = locale::strings(cx).get("settings.themeImport.deleteError");
                    window.push_notification(Notification::error(text), cx);
                }
                view.sync_themes(window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn filtered_themes(&self, cx: &Context<Self>) -> Vec<usize> {
        let variant = self.variant(cx);
        let needle = self.theme_search.read(cx).value().trim().to_lowercase();
        let mut indices: Vec<_> = self
            .themes
            .iter()
            .enumerate()
            .filter(|(_, row)| row.theme.palette.appearance == variant)
            .filter_map(|(index, row)| {
                theme_rank(&row.theme.name, &needle).map(|rank| (index, rank))
            })
            .collect();
        indices.sort_by_key(|(_, rank)| *rank);
        indices.into_iter().map(|(index, _)| index).collect()
    }

    fn theme_key(
        &mut self,
        index: usize,
        event: &gpui_kit::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke;
        if key.modifiers.alt || key.modifiers.platform || key.modifiers.shift {
            return;
        }
        if !key.modifiers.control && matches!(key.key.as_str(), "delete" | "backspace") {
            self.delete_theme(index, window, cx);
            cx.stop_propagation();
            return;
        }
        let rows = self.filtered_themes(cx);
        let Some(position) = rows.iter().position(|candidate| *candidate == index) else {
            return;
        };
        let next = match key.key.as_str() {
            "n" if key.modifiers.control => position.checked_add(1),
            "p" if key.modifiers.control => position.checked_sub(1),
            _ if key.modifiers.control => return,
            "down" => position.checked_add(1),
            "up" => position.checked_sub(1),
            "home" => Some(0),
            "end" => rows.len().checked_sub(1),
            _ => return,
        };
        if let Some(position) = next.filter(|next| *next < rows.len()) {
            let row = &self.themes[rows[position]];
            self.theme_focus_id = Some(row.theme.id.clone());
            self.preview = Some(row.theme.id.clone());
            window.focus(&row.focus, cx);
            self.theme_scroll.scroll_to_item(position);
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn preview_images(&mut self, preferences: &AppearancePreferences) -> [Arc<Image>; 3] {
        let shown = |variant| {
            self.preview
                .as_ref()
                .and_then(|id| {
                    self.themes
                        .iter()
                        .find(|row| row.theme.id == *id && row.theme.palette.appearance == variant)
                })
                .or_else(|| {
                    self.themes.iter().find(|row| {
                        row.theme.id == selected_theme(preferences, variant)
                            && row.theme.palette.appearance == variant
                    })
                })
                .or_else(|| {
                    self.themes
                        .iter()
                        .find(|row| row.theme.palette.appearance == variant)
                })
                .map(|row| row.theme.palette.clone())
                .unwrap_or_else(|| Palette::default_for(variant))
        };
        let light = &shown(Appearance::Light);
        let dark = &shown(Appearance::Dark);
        let key = (light.id.clone(), dark.id.clone());
        if self.schematics.as_ref().is_none_or(|(old, _)| *old != key) {
            self.schematics = Some((
                key,
                [
                    schematic::image(dark, Some(light)),
                    schematic::image(light, None),
                    schematic::image(dark, None),
                ],
            ));
        }
        self.schematics.as_ref().expect("prepared images").1.clone()
    }

    fn theme_list(
        &mut self,
        preferences: &AppearancePreferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let p = cx.global::<Palette>().clone();
        let direction = locale::strings(cx).direction();
        let variant = self.variant(cx);
        let rows = self.filtered_themes(cx);
        let wanted = selected_theme(preferences, variant);
        let selected = self
            .themes
            .iter()
            .find(|row| row.theme.id == wanted && row.theme.palette.appearance == variant)
            .or_else(|| {
                self.themes
                    .iter()
                    .find(|row| row.theme.palette.appearance == variant)
            })
            .map_or(wanted, |row| row.theme.id.as_str());
        let tab_stop = self
            .theme_focus_id
            .as_deref()
            .filter(|id| rows.iter().any(|i| self.themes[*i].theme.id == *id))
            .or_else(|| {
                rows.iter()
                    .find(|i| self.themes[**i].theme.id == selected)
                    .map(|_| selected)
            })
            .or_else(|| rows.first().map(|i| self.themes[*i].theme.id.as_str()));
        let mut list = div()
            .id("appearance-theme-list")
            .accessibility_id("appearance-theme-list")
            .role(Role::ListBox)
            .aria_label(locale::strings(cx).get(if variant == Appearance::Light {
                "settings.openchamber.visual.field.lightTheme"
            } else {
                "settings.openchamber.visual.field.darkTheme"
            }))
            .track_focus(&self.theme_list_focus)
            .max_h(gpui_kit::rems(20.5))
            .overflow_y_scroll()
            .track_scroll(&self.theme_scroll)
            .flex_row_from(direction)
            .flex_wrap()
            .gap_x(px(4.0))
            .px(px(4.0))
            .py(px(12.0))
            .rounded(px(12.0))
            .bg(p.foreground.opacity(if p.appearance == Appearance::Dark {
                0.03
            } else {
                0.04
            }))
            .on_hover(cx.listener(|view, hovered, _, cx| {
                if !hovered && view.preview.take().is_some() {
                    cx.notify();
                }
            }));
        if rows.is_empty() {
            let query = self.theme_search.read(cx).value();
            list = list.child(
                div()
                    .id("appearance-themes-empty")
                    .role(Role::Status)
                    .w_full()
                    .py(px(24.0))
                    .text_center()
                    .text_color(p.muted_foreground)
                    .child(locale::strings(cx).format(
                        "settings.openchamber.visual.field.noThemesMatch",
                        &[("query", query.trim())],
                    )),
            );
        }
        for index in rows.iter().copied() {
            let row = &self.themes[index];
            let active = row.theme.id == selected;
            let hovered_id = row.theme.id.clone();
            let focused = row.focus.is_focused(window);
            list = list.child(
                on_activate(
                    div()
                        .id(SharedString::from(format!(
                            "appearance-theme-{}",
                            row.theme.id
                        )))
                        .accessibility_id(format!("appearance-theme-{}", row.theme.id))
                        .role(Role::ListBoxOption)
                        .aria_label(row.theme.name.clone())
                        .aria_selected(active)
                        .track_focus(&row.focus)
                        .tab_index(if tab_stop == Some(row.theme.id.as_str()) {
                            0
                        } else {
                            -1
                        })
                        .flex_1()
                        .flex_basis(gpui_kit::rems(17.625))
                        .min_w_0()
                        .h(gpui_kit::rems(2.0))
                        .px(px(8.0))
                        .rounded(px(6.0))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(10.0))
                        .cursor_pointer()
                        .text_color(if active {
                            p.selection_foreground
                        } else {
                            p.muted_foreground
                        })
                        .when(active, |row| row.bg(p.selection.opacity(0.7)))
                        .when(!active && focused, |row| {
                            row.bg(p.hover).text_color(p.foreground)
                        })
                        .when(!active, |row| {
                            row.hover(move |style| style.bg(p.hover).text_color(p.foreground))
                        })
                        .on_hover(cx.listener(move |view, hovered, _, cx| {
                            if *hovered {
                                view.preview = Some(hovered_id.clone());
                                cx.notify();
                            }
                        }))
                        .child(
                            div()
                                .size(px(20.0))
                                .flex_shrink_0()
                                .rounded(px(6.0))
                                .bg(row.theme.palette.background)
                                .border_1()
                                .border_color(gpui_kit::hsla(
                                    0.0,
                                    0.0,
                                    if row.theme.palette.appearance == Appearance::Dark {
                                        1.0
                                    } else {
                                        0.0
                                    },
                                    0.12,
                                ))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .size(px(8.0))
                                        .rounded_full()
                                        .bg(row.theme.palette.primary),
                                ),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .when(direction == Direction::RightToLeft, |label| {
                                    label.text_right()
                                })
                                .child(row.theme.name.clone()),
                        )
                        .children(active.then(|| glyph("check", 16.0, p.muted_foreground)))
                        .children(
                            self.owner
                                .as_ref()
                                .is_some_and(|owner| owner.read(cx).is_custom_theme(&row.theme.id))
                                .then(|| {
                                    let label = locale::strings(cx).format(
                                        "settings.themeImport.delete",
                                        &[("name", &row.theme.name)],
                                    );
                                    on_activate(
                                        div()
                                            .id(SharedString::from(format!(
                                                "appearance-delete-{}",
                                                row.theme.id
                                            )))
                                            .role(Role::Button)
                                            .aria_label(label.clone())
                                            .size(px(24.0))
                                            .flex_shrink_0()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .when(self.deleting.contains(&row.theme.id), |button| {
                                                button.opacity(0.5)
                                            })
                                            .tooltip(move |window, cx| {
                                                Tooltip::new(label.clone()).build(window, cx)
                                            })
                                            .child(glyph("delete-bin", 16.0, p.muted_foreground)),
                                        cx,
                                        move |view, window, cx| {
                                            cx.stop_propagation();
                                            window.focus(&view.themes[index].focus, cx);
                                            view.delete_theme(index, window, cx);
                                        },
                                    )
                                    .test_support()
                                }),
                        ),
                    cx,
                    move |view, window, cx| {
                        view.theme_focus_id = Some(view.themes[index].theme.id.clone());
                        window.focus(&view.themes[index].focus, cx);
                        view.select_theme(index, cx);
                    },
                )
                .on_action(cx.listener(
                    move |view, action: &gpui_kit::base::actions::Confirm, _, cx| {
                        if !action.secondary {
                            view.select_theme(index, cx);
                            cx.stop_propagation();
                        }
                    },
                ))
                .on_key_down(cx.listener(move |view, event, window, cx| {
                    view.theme_key(index, event, window, cx)
                }))
                .test_support(),
            );
        }
        if rows.len() % 2 == 1 {
            list = list.child(div().flex_1().flex_basis(gpui_kit::rems(17.625)).h_0());
        }
        list.into_any_element()
    }

    fn theme_tabs(&self, cx: &Context<Self>) -> gpui_kit::AnyElement {
        let p = cx.global::<Palette>();
        let direction = locale::strings(cx).direction();
        let mut tabs = div()
            .flex_row_from(direction)
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(px(8.0))
            .bg(p.foreground.opacity(0.04));
        for (index, (variant, name, icon)) in [
            (Appearance::Light, "light", "sun"),
            (Appearance::Dark, "dark", "moon"),
        ]
        .into_iter()
        .enumerate()
        {
            let selected = variant == self.variant(cx);
            let label = locale::strings(cx).get(&format!(
                "settings.openchamber.visual.option.themeMode.{name}"
            ));
            tabs = tabs.child(
                on_activate(
                    div()
                        .id(SharedString::from(format!("appearance-{name}-theme")))
                        .accessibility_id(format!("appearance-{name}-theme"))
                        .role(Role::Button)
                        .aria_label(label.clone())
                        .aria_toggled(if selected {
                            gpui_kit::Toggled::True
                        } else {
                            gpui_kit::Toggled::False
                        })
                        .track_focus(&self.variant_focus[index])
                        .tab_index(0)
                        .anchor_scroll(Some(self.anchors[index].clone()))
                        .h(px(28.0))
                        .px(px(10.0))
                        .rounded(px(6.0))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(6.0))
                        .cursor_pointer()
                        .text_color(if selected {
                            p.foreground
                        } else {
                            p.muted_foreground
                        })
                        .when(selected, |tab| {
                            tab.bg(if p.appearance == Appearance::Dark {
                                p.elevated
                            } else {
                                p.background
                            })
                            .shadow(elevation::control(p))
                        })
                        .child(glyph(
                            icon,
                            14.0,
                            if selected {
                                p.foreground
                            } else {
                                p.muted_foreground
                            },
                        ))
                        .child(label),
                    cx,
                    move |view, _, cx| view.select_variant(variant, cx),
                )
                .on_action(cx.listener(
                    move |view, action: &gpui_kit::base::actions::Confirm, _, cx| {
                        if !action.secondary {
                            view.select_variant(variant, cx);
                            cx.stop_propagation();
                        }
                    },
                ))
                .test_support(),
            );
        }
        tabs.into_any_element()
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
            "appearance.color-mode" => 6,
            "appearance.import-theme" => 7,
            _ => return,
        };
        self.highlight = Some(item.to_owned());
        if index < 2 {
            self.select_variant(
                if index == 0 {
                    Appearance::Light
                } else {
                    Appearance::Dark
                },
                cx,
            );
        }
        cx.notify();
        // Wait until the newly selected Settings page has laid out its real rows.
        let entity = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = entity.update(cx, |view, cx| {
                view.anchors[index].scroll_to(window, cx);
                if index == 5 {
                    view.size.update(cx, |input, cx| input.focus(window, cx));
                } else if index < 2 {
                    window.focus(&view.variant_focus[index], cx);
                } else if index == 6 {
                    let mode = view
                        .owner
                        .as_ref()
                        .map_or(ThemeMode::System, |owner| owner.read(cx).preferences().mode);
                    window.focus(&view.mode_focus[mode_index(mode)], cx);
                } else if index == 7 {
                    window.focus(&view.import_focus, cx);
                } else {
                    view.pickers[index - 2].focus(window, cx);
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
        self.row(field.index() + 2, field.item(), cx)
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
            .when(divider, |title| title.pt(px(40.0)))
            .when(strings.direction() == Direction::RightToLeft, |title| {
                title.text_right()
            })
            .mb(px(20.0))
            .child(strings.get(key))
    }
}

impl Render for AppearanceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_themes(window, cx);
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
        let images = self.preview_images(&preferences);
        let mut radios = div()
            .id("appearance-color-mode")
            .accessibility_id("appearance.color-mode")
            .anchor_scroll(Some(self.anchors[6].clone()))
            .flex_row_from(direction)
            .gap(px(6.0))
            .p(px(4.0))
            .rounded(px(16.0))
            .bg(palette.foreground.opacity(0.04))
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
                        .tab_index(if selected { 0 } else { -1 })
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(2.0))
                        .px(px(6.0))
                        .pt(px(6.0))
                        .pb(px(2.0))
                        .rounded(px(12.0))
                        .text_color(if selected {
                            palette.foreground
                        } else {
                            palette.muted_foreground
                        })
                        .when(selected, |card| {
                            card.bg(if palette.appearance == Appearance::Dark {
                                palette.elevated
                            } else {
                                palette.background
                            })
                            .shadow(elevation::control(&palette))
                        })
                        .cursor_pointer()
                        .child(
                            img(images[index].clone())
                                .w_full()
                                .aspect_ratio(1.6)
                                .rounded(px(8.0)),
                        )
                        .child(div().w_full().truncate().text_center().child(label)),
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
        let theme_list = self.theme_list(&preferences, window, cx);
        let theme_tabs = self.theme_tabs(cx);
        let placeholder = locale::strings(cx).get("settings.themeImport.search");
        self.theme_search.update(cx, |input, cx| {
            input.set_placeholder(placeholder.clone(), window, cx)
        });
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
                _ => self
                    .owner
                    .as_ref()
                    .and_then(|owner| owner.read(cx).theme_error())
                    .map(|key| locale::strings(cx).get(key)),
            }
        };
        let retryable = matches!(status, Some(SaveStatus::Failed(_)))
            || self
                .owner
                .as_ref()
                .is_some_and(|owner| owner.read(cx).theme_error().is_some());
        let content = div()
            .max_w(px(840.0))
            .mx_auto()
            .text_left()
            .css_text(0.875, 1.25, cx)
            .child(crate::settings_view::page_header(
                crate::settings_navigation::Page::Appearance,
                cx,
            ))
            .child(Self::heading(
                "settings.openchamber.visual.section.colorMode",
                false,
                cx,
            ))
            .child(radios)
            .child(
                div()
                    .pt(px(16.0))
                    .mb(px(20.0))
                    .flex_row_from(direction)
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div().font_weight(FontWeight::MEDIUM).child(
                                    locale::strings(cx)
                                        .get("settings.openchamber.visual.section.themes"),
                                ),
                            )
                            .child(crate::settings_controls::info_hint(
                                "appearance-themes-info",
                                format!(
                                    "{}\n\n{}",
                                    locale::strings(cx).get(
                                        "settings.openchamber.visual.field.themeImportInfoTooltip"
                                    ),
                                    locale::strings(cx).get("settings.themeImport.catalogHint")
                                )
                                .into(),
                                cx,
                            )),
                    )
                    .child(on_activate(
                        gpui_kit::base::Button::new("appearance-import-theme")
                            .accessibility_id("appearance.import-theme")
                            .anchor_scroll(Some(self.anchors[7].clone()))
                            .accessibility_label(
                                locale::strings(cx).get("settings.themeImport.action"),
                            )
                            .track_focus(&self.import_focus)
                            .h(px(28.0))
                            .px(px(8.0))
                            .gap(px(6.0))
                            .rounded(px(6.0))
                            .bg(if palette.appearance == Appearance::Dark {
                                palette.elevated
                            } else {
                                palette.background
                            })
                            .shadow(elevation::control(&palette))
                            .disabled(self.owner.as_ref().is_none_or(|owner| {
                                owner.read(cx).theme_target().is_none()
                                    || owner.read(cx).custom_themes_loading()
                            }))
                            .child(glyph("search", 14.0, palette.muted_foreground))
                            .child(locale::strings(cx).get("settings.themeImport.action")),
                        cx,
                        |view, window, cx| {
                            if let Some(owner) = &view.owner {
                                catalog::open(owner.clone(), view.import_focus.clone(), window, cx);
                            }
                        },
                    )),
            )
            .child(
                div()
                    .flex_row_from(direction)
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .mb(px(12.0))
                    .child(theme_tabs)
                    .child(
                        Input::new(&self.theme_search)
                            .id("appearance-theme-search")
                            .accessibility_id("appearance-theme-search")
                            .aria_label(placeholder)
                            .w(px(224.0))
                            .h(px(28.0))
                            .focus_bordered(false),
                    ),
            )
            .child(theme_list)
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
            )
            .child(self.status_slot(error, retryable, cx));
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

fn mode_index(mode: ThemeMode) -> usize {
    match mode {
        ThemeMode::System => 0,
        ThemeMode::Light => 1,
        ThemeMode::Dark => 2,
    }
}

fn selected_theme(preferences: &AppearancePreferences, variant: Appearance) -> &str {
    match variant {
        Appearance::Light => &preferences.light_theme_id,
        Appearance::Dark => &preferences.dark_theme_id,
    }
}

fn theme_rank(name: &str, needle: &str) -> Option<u8> {
    let name = name.to_lowercase();
    if needle.is_empty() || name.starts_with(needle) {
        Some(0)
    } else if name
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '-' | '_'))
        .any(|word| word.starts_with(needle))
    {
        Some(1)
    } else if name.contains(needle) {
        Some(2)
    } else {
        None
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
