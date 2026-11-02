//! Retained Appearance pickers. Dismissal never confirms a highlighted value.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::component::IndexPath;
use gpui_kit::component::combobox::{Combobox, ComboboxEvent, ComboboxState};
use gpui_kit::component::searchable_list::{
    SearchableListChange, SearchableListDelegate, SearchableListItem, SearchableVec,
};
use gpui_kit::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, RenderOnce, Role, SharedString, StatefulInteractiveElement, StyleRefinement,
    Styled, Subscription, TestSupportExt, WeakEntity, Window, div, prelude::FluentBuilder, px,
};

use super::{AppearanceView, Field};
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::elevation;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;

#[derive(Clone)]
pub(super) struct OptionItem {
    pub value: String,
    pub label: SharedString,
    pub id: SharedString,
    pub field: Field,
    pub current: bool,
    pub owner: WeakEntity<AppearanceView>,
}

impl PartialEq for OptionItem {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
            && self.label == other.label
            && self.id == other.id
            && self.current == other.current
    }
}
impl Eq for OptionItem {}

impl SearchableListItem for OptionItem {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone()
    }
    fn value(&self) -> &String {
        &self.value
    }
    fn matches(&self, query: &str) -> bool {
        let text = format!("{} {}", self.label, self.value).to_lowercase();
        query
            .to_lowercase()
            .split_whitespace()
            .all(|word| text.contains(word))
    }
    fn render(&self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let owner = self.owner.clone();
        let value = self.value.clone();
        let field = self.field;
        div()
            .id(self.id.clone())
            .accessibility_id(self.id.clone())
            .role(Role::RadioButton)
            .aria_label(self.label.clone())
            .aria_toggled(if self.current {
                gpui_kit::Toggled::True
            } else {
                gpui_kit::Toggled::False
            })
            .on_a11y_action(gpui_kit::AccessibleAction::Click, move |_, window, cx| {
                let _ = owner.update(cx, |view, cx| view.choose(field, &value, cx));
                window.dispatch_action(Box::new(gpui_kit::base::actions::Cancel), cx);
            })
            .min_w_0()
            .truncate()
            .css_text(0.875, 1.25, cx)
            .child(self.label.clone())
            .test_support()
    }
}

struct Items {
    items: SearchableVec<OptionItem>,
    confirmed: Rc<RefCell<Option<String>>>,
}

impl SearchableListDelegate for Items {
    type Item = OptionItem;
    fn items_count(&self, section: usize) -> usize {
        self.items.items_count(section)
    }
    fn item(&self, index: IndexPath) -> Option<&OptionItem> {
        self.items.item(index)
    }
    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SearchableListItem<Value = V>,
        V: PartialEq,
    {
        self.items.position(value)
    }
    fn perform_search(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> gpui_kit::Task<()> {
        self.items.perform_search(query, window, cx)
    }
    fn on_will_change(
        &mut self,
        selection: &mut Vec<(IndexPath, OptionItem)>,
        changes: &[SearchableListChange],
    ) {
        self.items.on_will_change(selection, changes);
        *self.confirmed.borrow_mut() = selection.first().map(|(_, item)| item.value.clone());
    }
}

pub(super) struct Picker {
    state: Entity<ComboboxState<Items>>,
    trigger_focus: FocusHandle,
    items: Vec<OptionItem>,
    value: String,
    confirmed: Rc<RefCell<Option<String>>>,
    _subscription: Subscription,
}

impl Picker {
    pub fn new(field: Field, window: &mut Window, cx: &mut Context<AppearanceView>) -> Self {
        let confirmed = Rc::new(RefCell::new(None));
        let state = cx.new(|cx| {
            ComboboxState::new(
                Items {
                    items: SearchableVec::new(Vec::<OptionItem>::new()),
                    confirmed: confirmed.clone(),
                },
                vec![],
                window,
                cx,
            )
            .searchable(true)
            .confirm_on_reselect(true)
        });
        let confirmation = confirmed.clone();
        let trigger_focus = state.read(cx).focus_handle(cx);
        let subscription = cx.subscribe_in(&state, window, move |view, _, event, _, cx| {
            if matches!(event, ComboboxEvent::Confirm(_))
                && let Some(value) = confirmation.borrow_mut().take()
            {
                view.choose(field, &value, cx);
            }
        });
        Self {
            state,
            trigger_focus,
            items: Vec::new(),
            value: String::new(),
            confirmed,
            _subscription: subscription,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.state.update(cx, |state, cx| state.focus(window, cx));
    }

    pub fn control(
        &mut self,
        field: Field,
        items: Vec<OptionItem>,
        value: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> Control {
        if self.items != items || self.value != value {
            self.state.update(cx, |state, cx| {
                if self.items != items {
                    state.set_items(
                        Items {
                            items: SearchableVec::new(items.clone()),
                            confirmed: self.confirmed.clone(),
                        },
                        window,
                        cx,
                    );
                }
                state.set_selected_values(&[value.to_owned()], window, cx);
            });
            self.items = items;
            self.value = value.to_owned();
        }
        let label = self
            .items
            .iter()
            .find(|item| item.value == value)
            .map(|item| item.label.clone())
            .unwrap_or_else(|| value.to_owned().into());
        Control {
            state: self.state.clone(),
            trigger_focus: self.trigger_focus.clone(),
            field,
            label,
        }
    }
}

#[derive(IntoElement)]
pub(super) struct Control {
    state: Entity<ComboboxState<Items>>,
    trigger_focus: FocusHandle,
    field: Field,
    label: SharedString,
}

impl RenderOnce for Control {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = cx.global::<Palette>().clone();
        let hover = palette.hover;
        let direction = locale::strings(cx).direction();
        let aria = locale::strings(cx).get(self.field.aria());
        let search = locale::strings(cx).get(if matches!(self.field, Field::Light | Field::Dark) {
            "settings.openchamber.visual.field.searchThemes"
        } else {
            "settings.view.search.placeholder"
        });
        let empty = locale::strings(cx).get("commandPalette.empty.noResults");
        let state = self.state.clone();
        div()
            .key_context("SettingsPicker")
            .h(gpui_kit::rems(2.0))
            .child(
                Combobox::new(&self.state)
                    .appearance(false)
                    .border_0()
                    .p_0()
                    .w_auto()
                    .h(gpui_kit::rems(2.0))
                    .menu_width(px(280.0))
                    .menu_max_h(px(300.0))
                    .menu_offset(px(4.0))
                    .menu_align(if direction == Direction::RightToLeft {
                        gpui_kit::base::Align::End
                    } else {
                        gpui_kit::base::Align::Start
                    })
                    .menu_style(
                        StyleRefinement::default()
                            .border_0()
                            .rounded(px(10.0))
                            .bg(palette.elevated)
                            .shadow(elevation::floating(&palette)),
                    )
                    .search_clearable(false)
                    .search_placeholder(search)
                    .empty(move |_, _| div().p(px(12.0)).child(empty.clone()))
                    .check_icon(
                        gpui_kit::component::Icon::default()
                            .path(crate::assets::icon_path("check")),
                    )
                    .render_trigger(move |trigger, _, cx| {
                        let open = trigger.is_open();
                        let state = state.clone();
                        let activate = Rc::new(move |window: &mut Window, cx: &mut App| {
                            cx.stop_propagation();
                            state.update(cx, |state, cx| state.focus(window, cx));
                            if open {
                                window
                                    .dispatch_action(Box::new(gpui_kit::base::actions::Cancel), cx);
                            } else {
                                window.dispatch_action(
                                    Box::new(gpui_kit::base::actions::Confirm { secondary: false }),
                                    cx,
                                );
                            }
                        });
                        let accessible_activate = activate.clone();
                        div()
                            .id(self.field.id())
                            .accessibility_id(self.field.id())
                            .role(Role::Button)
                            .aria_label(aria.clone())
                            .track_focus(&self.trigger_focus)
                            .aria_expanded(open)
                            .on_click(move |_, window, cx| activate(window, cx))
                            .on_a11y_action(
                                gpui_kit::AccessibleAction::Click,
                                move |_, window, cx| accessible_activate(window, cx),
                            )
                            .h(gpui_kit::rems(2.0))
                            .max_w(px(280.0))
                            .px(px(12.0))
                            .rounded(px(6.0))
                            .bg(if palette.appearance == crate::theme::Appearance::Light {
                                palette.background
                            } else {
                                palette.elevated
                            })
                            .shadow(elevation::control(&palette))
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(8.0))
                            .relative()
                            .when(
                                palette.appearance == crate::theme::Appearance::Dark,
                                |control| {
                                    let sheen = palette.foreground.opacity(0.01);
                                    control.child(div().absolute().inset_0().rounded(px(6.0)).bg(
                                        gpui_kit::linear_gradient(
                                            180.0,
                                            gpui_kit::linear_color_stop(sheen, 0.0),
                                            gpui_kit::linear_color_stop(sheen.opacity(0.0), 0.5),
                                        ),
                                    ))
                                },
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_left()
                                    .when(direction == Direction::RightToLeft, |label| {
                                        label.text_right()
                                    })
                                    .css_text(0.875, 1.25, cx)
                                    .child(self.label.clone()),
                            )
                            .child(glyph("arrow-down-s", 14.0, palette.muted_foreground))
                            .test_support()
                    }),
            )
    }
}
