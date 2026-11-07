//! The session row menu, the inline rename editor and the removal dialog.
//!
//! Mirrors `SessionNodeItem.tsx`: the row's More button opens the menu at
//! the row's end, a secondary click opens it at the pointer, and both list
//! the same items. Pointer, keyboard and accessibility activation run the
//! same `Workspace` method. Closing the menu returns focus to where it was,
//! except into a rename, whose field takes focus.

use gpui_kit::component::WindowExt;
use std::time::Duration;

use gpui_kit::base::actions::{
    Cancel, Confirm, SelectDown, SelectFirst, SelectLast, SelectLeft, SelectRight, SelectUp,
};
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    Anchor, AnchoredPositionMode, AnyElement, AppContext, Context, Edges, Entity,
    EntityInputHandler, FocusHandle, Hsla, InteractiveElement, IntoElement, ParentElement, Pixels,
    Point, Role, SharedString, StatefulInteractiveElement, Styled, Subscription, TestSupportExt,
    Window, anchored, deferred, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::session_actions::{
    self, MenuContext, MenuEntry, SessionAction, Submenu, SubmenuEntry, SubmenuLabel,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::elevation;
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::session_management::RemovalConfirm;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// `typography-ui-label`.
const LABEL_SIZE: f32 = 0.843_75;
const LABEL_LINE: f32 = 1.25;
const MENU_MIN_WIDTH: f32 = 180.0;
/// `SessionMenuItemHint`: resting on an item explains it.
const HINT_DELAY: Duration = Duration::from_millis(500);
/// The key context of the open menu; `workspace_view::bind_keys` binds it.
pub(crate) const MENU_CONTEXT: &str = "SessionMenu";
/// Accessibility identity of the open menu.
pub(crate) const MENU_ID: &str = "session-menu";
/// Accessibility identity of the open submenu.
pub(crate) const SUBMENU_ID: &str = "session-submenu";
/// `typography-micro`.
const MICRO_SIZE: f32 = 0.75;
const MICRO_LINE: f32 = 1.333_333;

/// An open row menu.
pub(crate) struct SessionMenu {
    pub(crate) row_key: SharedString,
    pub(crate) session_id: SessionId,
    pub(crate) entries: Vec<MenuEntry>,
    /// What the items were decided from; their hints read it too.
    pub(crate) context: MenuContext,
    pub(crate) highlighted: Option<usize>,
    /// Where a secondary click opened it; `None` opens below the trigger.
    pub(crate) position: Option<Point<Pixels>>,
    pub(crate) focus: FocusHandle,
    /// What had focus before the menu opened.
    return_focus: Option<FocusHandle>,
    pub(crate) submenu: Option<OpenSubmenu>,
}

/// The submenu beside an item of the open menu.
pub(crate) struct OpenSubmenu {
    pub(crate) kind: Submenu,
    /// The index of the item that opened it.
    pub(crate) trigger: usize,
    pub(crate) entries: Vec<SubmenuEntry>,
    pub(crate) highlighted: Option<usize>,
    /// The arrow keys move through the submenu instead of the menu.
    pub(crate) keyboard: bool,
}

/// An inline rename in progress.
pub(crate) struct SessionRename {
    pub(crate) row_key: SharedString,
    pub(crate) session_id: SessionId,
    pub(crate) input: Entity<InputState>,
    _subscription: Subscription,
}

impl Workspace {
    /// Opens the menu of a session row. `position` is the pointer for a
    /// secondary click; keyboard opening highlights the first item.
    pub(crate) fn open_session_menu(
        &mut self,
        row_key: SharedString,
        session_id: SessionId,
        position: Option<Point<Pixels>>,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self
            .sidebar_items
            .iter()
            .find(|item| item.key() == row_key.as_ref())
        else {
            return;
        };
        let Some(context) = self.session_menu_context(item) else {
            return;
        };
        if self
            .session_menu
            .as_ref()
            .is_some_and(|menu| menu.row_key == row_key && menu.position == position)
        {
            self.close_session_menu(true, window, cx);
            return;
        }
        let entries = session_actions::menu_entries(context);
        let return_focus = self
            .session_menu
            .take()
            .and_then(|menu| menu.return_focus)
            .or_else(|| window.focused(cx));
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.session_menu = Some(SessionMenu {
            highlighted: keyboard
                .then(|| session_actions::edge_highlight(&entries, false))
                .flatten(),
            row_key,
            session_id: session_id.clone(),
            entries,
            context,
            position,
            focus,
            return_focus,
            submenu: None,
        });
        self.session_menu_opened(&session_id, cx);
        cx.notify();
    }

    /// Rebuilds the open menu after what its items depend on changed, as
    /// when the AI rename check answers. The highlight stays on its action.
    pub(crate) fn refresh_session_menu(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.session_menu.as_ref() else {
            return;
        };
        let Some(context) = self
            .sidebar_items
            .iter()
            .find(|item| item.key() == menu.row_key.as_ref())
            .and_then(|item| self.session_menu_context(item))
        else {
            return;
        };
        let entries = session_actions::menu_entries(context);
        let Some(menu) = self.session_menu.as_mut() else {
            return;
        };
        let at = |entries: &[MenuEntry], action: Option<SessionAction>| {
            action.and_then(|action| {
                entries
                    .iter()
                    .position(|entry| entry.action() == Some(action) && entry.is_enabled())
            })
        };
        let highlighted = menu
            .highlighted
            .and_then(|index| menu.entries.get(index))
            .and_then(|entry| entry.action());
        menu.highlighted = at(&entries, highlighted);
        if let Some(open) = menu.submenu.as_mut() {
            let trigger = menu
                .entries
                .get(open.trigger)
                .and_then(|entry| entry.action());
            match at(&entries, trigger) {
                Some(index) => open.trigger = index,
                None => menu.submenu = None,
            }
        }
        menu.context = context;
        menu.entries = entries;
        cx.notify();
    }

    pub(crate) fn close_session_menu(
        &mut self,
        restore_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.session_menu.take() else {
            return;
        };
        self.session_menu_closed();
        if restore_focus && let Some(focus) = menu.return_focus {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Runs the item at `index` of the open menu, or opens its submenu.
    pub(crate) fn activate_session_menu_item(
        &mut self,
        index: usize,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.session_menu.as_ref() else {
            return;
        };
        let Some(MenuEntry::Item {
            action,
            enabled: true,
        }) = menu.entries.get(index).copied()
        else {
            return;
        };
        if let Some(kind) = action.submenu() {
            self.open_session_submenu(index, kind, keyboard, cx);
            return;
        }
        let row_key = menu.row_key.clone();
        let session_id = menu.session_id.clone();
        // A rename takes focus into its field; anything else returns it.
        self.close_session_menu(action != SessionAction::Rename, window, cx);
        self.run_session_action(row_key, session_id, action, window, cx);
    }

    /// Opens the submenu of the item at `trigger`. From the keyboard its
    /// first enabled item is highlighted and the arrows move inside it.
    pub(crate) fn open_session_submenu(
        &mut self,
        trigger: usize,
        kind: Submenu,
        keyboard: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.session_menu.as_ref() else {
            return;
        };
        if let Some(open) = &menu.submenu
            && open.trigger == trigger
        {
            if keyboard
                && let Some(open) = self
                    .session_menu
                    .as_mut()
                    .and_then(|menu| menu.submenu.as_mut())
            {
                open.keyboard = true;
                open.highlighted = open.highlighted.or_else(|| first_enabled(&open.entries));
                cx.notify();
            }
            return;
        }
        let session_id = menu.session_id.clone();
        let entries = self.session_submenu_entries(kind, &session_id, cx);
        if let Some(menu) = self.session_menu.as_mut() {
            menu.highlighted = Some(trigger);
            menu.submenu = Some(OpenSubmenu {
                kind,
                trigger,
                highlighted: keyboard.then(|| first_enabled(&entries)).flatten(),
                entries,
                keyboard,
            });
        }
        cx.notify();
    }

    /// Rebuilds an open submenu of `kind` after what it lists changed, as
    /// when a worktree read answers. The highlight stays on the same item.
    pub(crate) fn refresh_session_submenu(&mut self, kind: Submenu, cx: &mut Context<Self>) {
        let Some(menu) = self.session_menu.as_ref() else {
            return;
        };
        if menu.submenu.as_ref().is_none_or(|open| open.kind != kind) {
            return;
        }
        let session_id = menu.session_id.clone();
        let entries = self.session_submenu_entries(kind, &session_id, cx);
        if let Some(open) = self
            .session_menu
            .as_mut()
            .and_then(|menu| menu.submenu.as_mut())
        {
            let highlighted = open
                .highlighted
                .and_then(|index| open.entries.get(index))
                .and_then(SubmenuEntry::action)
                .cloned();
            open.highlighted = highlighted
                .and_then(|action| {
                    entries
                        .iter()
                        .position(|entry| entry.action() == Some(&action) && entry.is_enabled())
                })
                .or_else(|| open.keyboard.then(|| first_enabled(&entries)).flatten());
            open.entries = entries;
            cx.notify();
        }
    }

    fn close_session_submenu(&mut self, cx: &mut Context<Self>) {
        if let Some(menu) = self.session_menu.as_mut()
            && let Some(open) = menu.submenu.take()
        {
            menu.highlighted = Some(open.trigger);
            cx.notify();
        }
    }

    /// Runs the submenu item at `index`.
    pub(crate) fn activate_session_submenu_item(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.session_menu.as_ref() else {
            return;
        };
        let Some(SubmenuEntry::Item(item)) = menu
            .submenu
            .as_ref()
            .and_then(|open| open.entries.get(index))
        else {
            return;
        };
        if !item.enabled {
            return;
        }
        let action = item.action.clone();
        let row_key = menu.row_key.clone();
        let session_id = menu.session_id.clone();
        self.close_session_menu(true, window, cx);
        self.run_session_submenu_action(row_key, session_id, action, window, cx);
    }

    fn step_session_menu(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(menu) = self.session_menu.as_mut() else {
            return;
        };
        match menu.submenu.as_mut() {
            Some(open) if open.keyboard => {
                let entries = &open.entries;
                open.highlighted = session_actions::step_highlight_by(
                    entries.len(),
                    |index| entries[index].is_enabled(),
                    open.highlighted,
                    step,
                );
            }
            _ => {
                menu.submenu = None;
                menu.highlighted =
                    session_actions::step_highlight(&menu.entries, menu.highlighted, step);
            }
        }
        cx.notify();
    }

    fn edge_session_menu(&mut self, last: bool, cx: &mut Context<Self>) {
        let Some(menu) = self.session_menu.as_mut() else {
            return;
        };
        match menu.submenu.as_mut() {
            Some(open) if open.keyboard => {
                let enabled = |entry: &SubmenuEntry| entry.is_enabled();
                open.highlighted = if last {
                    open.entries.iter().rposition(enabled)
                } else {
                    open.entries.iter().position(enabled)
                };
            }
            _ => {
                menu.submenu = None;
                menu.highlighted = session_actions::edge_highlight(&menu.entries, last);
            }
        }
        cx.notify();
    }

    /// The arrow toward the line's end opens a submenu; the one toward its
    /// start closes it. The layout direction decides which arrow is which.
    fn horizontal_session_menu(
        &mut self,
        toward_end: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.session_menu.as_ref() else {
            return;
        };
        let in_submenu = menu.submenu.as_ref().is_some_and(|open| open.keyboard);
        if in_submenu {
            if !toward_end {
                self.close_session_submenu(cx);
            }
            return;
        }
        if toward_end && let Some(index) = menu.highlighted {
            let opens = menu
                .entries
                .get(index)
                .and_then(|entry| entry.action())
                .is_some_and(|action| action.submenu().is_some());
            if opens {
                self.activate_session_menu_item(index, true, window, cx);
            }
        }
    }

    fn confirm_session_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.session_menu.as_ref() else {
            return;
        };
        match menu.submenu.as_ref() {
            Some(open) if open.keyboard => {
                if let Some(index) = open.highlighted {
                    self.activate_session_submenu_item(index, window, cx);
                }
            }
            _ => {
                if let Some(index) = menu.highlighted {
                    self.activate_session_menu_item(index, true, window, cx);
                }
            }
        }
    }

    /// Escape leaves a keyboard submenu first, then the menu.
    fn cancel_session_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .session_menu
            .as_ref()
            .and_then(|menu| menu.submenu.as_ref())
            .is_some_and(|open| open.keyboard)
        {
            self.close_session_submenu(cx);
            return;
        }
        self.close_session_menu(true, window, cx);
    }

    /// A press outside the menu closes it. When that press lands on the
    /// menu's own trigger, its click finds the same press recorded and
    /// leaves the menu closed, as a toggle.
    fn dismiss_session_menu(
        &mut self,
        press: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(menu) = self.session_menu.as_ref() {
            self.session_management.menu_dismissed = Some((menu.row_key.clone(), press));
        }
        self.close_session_menu(false, window, cx);
    }

    /// The open menu, if it belongs to `row_key`, anchored at the row's
    /// trigger or at the pointer.
    pub(crate) fn render_session_menu(
        &self,
        row_key: &str,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self
            .session_menu
            .as_ref()
            .filter(|menu| menu.row_key.as_ref() == row_key)?;
        let strings = locale::strings(cx).clone();
        let context = menu.context;
        let items = menu
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let MenuEntry::Item { action, enabled } = *entry else {
                    return menu_separator(palette);
                };
                let highlighted = menu.highlighted == Some(index);
                let opens = action.submenu();
                let row = menu_row(
                    MenuRow {
                        id: format!("session-menu-item:{}", action.id()),
                        label: strings.get(action.label_key()),
                        hint: action.hint_key(&context).map(|key| strings.get(key)),
                        icon: Some(action.icon()),
                        enabled,
                        highlighted,
                        destructive: action.is_destructive(),
                        checked: None,
                        opens_submenu: opens.is_some(),
                    },
                    palette,
                    direction,
                    cx,
                )
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    let Some(menu) = this.session_menu.as_mut() else {
                        return;
                    };
                    if !enabled {
                        return;
                    }
                    let submenu_here = menu
                        .submenu
                        .as_ref()
                        .is_some_and(|open| open.trigger == index);
                    if menu.highlighted == Some(index) && (opens.is_none() || submenu_here) {
                        return;
                    }
                    menu.highlighted = Some(index);
                    match opens {
                        Some(kind) => this.open_session_submenu(index, kind, false, cx),
                        None => {
                            menu.submenu = None;
                            cx.notify();
                        }
                    }
                }));
                let submenu = menu
                    .submenu
                    .as_ref()
                    .filter(|open| open.trigger == index)
                    .map(|open| {
                        self.render_session_submenu(open, palette, direction, &strings, cx)
                    });
                let row = if enabled {
                    on_activate(row, cx, move |this, window, cx| {
                        this.activate_session_menu_item(index, false, window, cx);
                    })
                    .test_support()
                    .into_any_element()
                } else {
                    row.test_support().into_any_element()
                };
                div()
                    .relative()
                    .child(row)
                    .children(submenu)
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let popup = div()
            .id(MENU_ID)
            .role(Role::Menu)
            .accessibility_id(MENU_ID)
            .aria_label(strings.get("sessions.sidebar.session.menu.label"))
            .track_focus(&menu.focus)
            .key_context(MENU_CONTEXT)
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step_session_menu(1, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step_session_menu(-1, cx)))
            .on_action(
                cx.listener(|this, _: &SelectFirst, _, cx| this.edge_session_menu(false, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.edge_session_menu(true, cx)))
            .on_action(cx.listener(move |this, _: &SelectRight, window, cx| {
                this.horizontal_session_menu(direction == Direction::LeftToRight, window, cx)
            }))
            .on_action(cx.listener(move |this, _: &SelectLeft, window, cx| {
                this.horizontal_session_menu(direction == Direction::RightToLeft, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &Confirm, window, cx| this.confirm_session_menu(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &Cancel, window, cx| this.cancel_session_menu(window, cx)),
            )
            .on_mouse_down_out(
                cx.listener(|this, event: &gpui_kit::MouseDownEvent, window, cx| {
                    // A press inside the open submenu is the submenu's.
                    let submenu_open = this
                        .session_menu
                        .as_ref()
                        .is_some_and(|menu| menu.submenu.is_some());
                    if submenu_open
                        && this
                            .session_management
                            .submenu_bounds
                            .is_some_and(|bounds| bounds.contains(&event.position))
                    {
                        return;
                    }
                    this.dismiss_session_menu(event.position, window, cx)
                }),
            )
            .occlude()
            .min_w(px(MENU_MIN_WIDTH))
            .p(px(4.0))
            .rounded(px(12.0))
            .bg(palette.elevated)
            .shadow(elevation::floating(palette))
            .flex()
            .flex_col()
            .children(items)
            .test_support();
        let anchor = match (menu.position.is_some(), direction) {
            (true, Direction::LeftToRight) | (false, Direction::RightToLeft) => Anchor::TopLeft,
            (true, Direction::RightToLeft) | (false, Direction::LeftToRight) => Anchor::TopRight,
        };
        let mut placed = anchored()
            .anchor(anchor)
            .snap_to_window_with_margin(Edges::all(px(8.0)));
        if let Some(position) = menu.position {
            placed = placed
                .position(position)
                .position_mode(AnchoredPositionMode::Window);
        }
        Some(
            deferred(
                placed.child(
                    div()
                        .when(menu.position.is_none(), |this| this.mt(px(4.0)))
                        .child(popup),
                ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// The submenu beside its trigger's end edge, at the trigger's top.
    fn render_session_submenu(
        &self,
        open: &OpenSubmenu,
        palette: &Palette,
        direction: Direction,
        strings: &Strings,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let items = open
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| match entry {
                SubmenuEntry::Separator => menu_separator(palette),
                SubmenuEntry::Note(key) => div()
                    .id(SharedString::from(format!("session-submenu-note:{key}")))
                    .role(Role::MenuItem)
                    .accessibility_id(format!("session-submenu-note:{key}"))
                    .aria_label(strings.get(key))
                    .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
                    .px(px(8.0))
                    .py(px(2.0))
                    .css_text(MICRO_SIZE, MICRO_LINE, cx)
                    .text_color(palette.muted_foreground)
                    .child(strings.get(key))
                    .into_any_element(),
                SubmenuEntry::Item(item) => {
                    let label = match &item.label {
                        SubmenuLabel::Key(key) => strings.get(key),
                        SubmenuLabel::Text(text) => SharedString::from(text.clone()),
                    };
                    let enabled = item.enabled;
                    let row = menu_row(
                        MenuRow {
                            id: format!("session-submenu-item:{}", item.action.id()),
                            label,
                            hint: item.title.clone().map(SharedString::from),
                            icon: item.icon,
                            enabled,
                            highlighted: open.highlighted == Some(index),
                            destructive: item.destructive,
                            checked: Some(item.checked),
                            opens_submenu: false,
                        },
                        palette,
                        direction,
                        cx,
                    )
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if let Some(open) = this
                            .session_menu
                            .as_mut()
                            .and_then(|menu| menu.submenu.as_mut())
                            && enabled
                            && open.highlighted != Some(index)
                        {
                            open.highlighted = Some(index);
                            cx.notify();
                        }
                    }));
                    if enabled {
                        on_activate(row, cx, move |this, window, cx| {
                            this.activate_session_submenu_item(index, window, cx);
                        })
                        .test_support()
                        .into_any_element()
                    } else {
                        row.test_support().into_any_element()
                    }
                }
            })
            .collect::<Vec<_>>();
        let workspace = cx.entity().downgrade();
        let panel = div()
            .id(SUBMENU_ID)
            .role(Role::Menu)
            .accessibility_id(SUBMENU_ID)
            .occlude()
            .min_w(px(submenu_min_width(open.kind)))
            .p(px(4.0))
            .rounded(px(12.0))
            .bg(palette.elevated)
            .shadow(elevation::floating(palette))
            .flex()
            .flex_col()
            .children(items)
            .child(
                // Records the panel's bounds so a press inside it does not
                // count as outside the menu.
                gpui_kit::canvas(
                    move |bounds, _, cx| {
                        let _ = workspace.update(cx, |this, _| {
                            this.session_management.submenu_bounds = Some(bounds);
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .test_support();
        let anchor = match direction {
            Direction::LeftToRight => Anchor::TopLeft,
            Direction::RightToLeft => Anchor::TopRight,
        };
        div()
            .absolute()
            .top(px(-4.0))
            .inset_end(direction, px(0.0))
            .size_0()
            .child(
                deferred(
                    anchored()
                        .anchor(anchor)
                        .snap_to_window_with_margin(Edges::all(px(8.0)))
                        .child(div().margin_start(direction, px(8.0)).child(panel)),
                )
                .with_priority(2),
            )
            .into_any_element()
    }

    /// The row's More button: shown on hover, on focus and while its menu is
    /// open, at the row's end. The menu anchors to it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_session_menu_trigger(
        &self,
        row_key: &SharedString,
        session_id: &SessionId,
        menu_open: bool,
        row_group: &SharedString,
        palette: &Palette,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let id = format!("{row_key}:menu");
        let focus = window
            .use_keyed_state(SharedString::from(format!("{id}-focus")), cx, |_, cx| {
                cx.focus_handle().tab_index(0).tab_stop(true)
            })
            .read(cx)
            .clone();
        let foreground = palette.foreground;
        let button_group = SharedString::from(id.clone());
        let open_key = row_key.clone();
        let open_id = session_id.clone();
        let accessible_key = row_key.clone();
        let accessible_id = session_id.clone();
        let workspace = cx.entity().downgrade();
        let button = div()
            .id(SharedString::from(id.clone()))
            .group(button_group.clone())
            .role(Role::Button)
            .accessibility_id(id)
            .aria_label(strings.get("sessions.sidebar.session.menu.label"))
            .aria_expanded(
                menu_open
                    && self
                        .session_menu
                        .as_ref()
                        .is_some_and(|menu| menu.position.is_none()),
            )
            .size(px(16.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(!menu_open, |this| {
                this.opacity(0.0)
                    .group_hover(row_group.clone(), |style| style.opacity(1.0))
                    .focus(|style| style.opacity(1.0))
            })
            // A press here is the button's, never the row's.
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .on_click(
                cx.listener(move |this, event: &gpui_kit::ClickEvent, window, cx| {
                    cx.stop_propagation();
                    let dismissed = this.session_management.menu_dismissed.take();
                    let press = match event {
                        gpui_kit::ClickEvent::Mouse(click) => Some(click.down.position),
                        _ => None,
                    };
                    if dismissed.is_some_and(|(key, at)| key == open_key && Some(at) == press) {
                        return;
                    }
                    this.open_session_menu(
                        open_key.clone(),
                        open_id.clone(),
                        None,
                        event.is_keyboard(),
                        window,
                        cx,
                    );
                }),
            )
            .on_a11y_action(gpui_kit::AccessibleAction::Click, move |_, window, cx| {
                let _ = workspace.update(cx, |this, cx| {
                    this.open_session_menu(
                        accessible_key.clone(),
                        accessible_id.clone(),
                        None,
                        true,
                        window,
                        cx,
                    )
                });
            })
            .child(
                glyph("more-2", 10.0, palette.muted_foreground)
                    .group_hover(button_group, move |style| style.text_color(foreground)),
            )
            .test_support()
            .track_focus(&focus);
        let menu = self.render_session_menu(row_key, palette, direction, cx);
        div()
            .absolute()
            .top(px(8.0))
            .inset_end(direction, px(4.0))
            .child(button)
            // The menu's corner sits at the button's bottom end.
            .children(menu.map(|menu| {
                div()
                    .absolute()
                    .top(px(16.0))
                    .inset_end(direction, px(0.0))
                    .child(menu)
            }))
            .into_any_element()
    }

    /// Opens the rename field over a row's title, holding the whole title
    /// selected so the first keystroke replaces it.
    pub(crate) fn start_rename(
        &mut self,
        row_key: SharedString,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A second title while one is in flight would be dropped on save.
        if self
            .session_management
            .renaming
            .contains(&self.mutation_key(&session_id))
        {
            return;
        }
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let title = if session.title.is_empty() {
            locale::strings(cx)
                .get("sessions.sidebar.session.untitled")
                .to_string()
        } else {
            session.title.clone()
        };
        let placeholder = locale::strings(cx).get("sessions.sidebar.session.menu.rename");
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        input.update(cx, |input, cx| {
            input.set_value(title, window, cx);
            input.select_all(window, cx);
            input.focus(window, cx);
        });
        let subscription = cx.subscribe_in(&input, window, |this, input, event, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                let composing = input.update(cx, |input, cx| {
                    input.marked_text_range(window, cx).is_some()
                });
                if !composing {
                    this.save_rename(window, cx);
                }
            }
        });
        self.session_rename = Some(SessionRename {
            row_key,
            session_id,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Commits the rename field. An empty or unchanged title closes it
    /// without a request.
    pub(crate) fn save_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.session_rename.take() else {
            return;
        };
        let draft = rename.input.read(cx).value().to_string();
        let current = self
            .sessions
            .get(&rename.session_id)
            .map(|session| session.title.clone())
            .unwrap_or_default();
        if let Some(title) = session_actions::rename_title(&current, &draft) {
            self.commit_rename(rename.session_id, title, window, cx);
        }
        self.focus_composer(window, cx);
        cx.notify();
    }

    pub(crate) fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_rename.take().is_some() {
            self.focus_composer(window, cx);
            cx.notify();
        }
    }

    /// The rename field and its Save and Cancel buttons, one text line tall.
    pub(crate) fn render_rename(
        &self,
        row_key: &str,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let rename = self
            .session_rename
            .as_ref()
            .filter(|rename| rename.row_key.as_ref() == row_key)?;
        let strings = locale::strings(cx).clone();
        let input = rename.input.clone();
        let button = |id: String, icon: &'static str, label: SharedString, palette: &Palette| {
            let foreground = palette.foreground;
            div()
                .id(SharedString::from(id.clone()))
                .group(SharedString::from(id.clone()))
                .role(Role::Button)
                .accessibility_id(id.clone())
                .aria_label(label.clone())
                .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
                .flex_shrink_0()
                .cursor_pointer()
                .child(
                    glyph(icon, 14.0, palette.muted_foreground)
                        .group_hover(SharedString::from(id), move |style| {
                            style.text_color(foreground)
                        }),
                )
        };
        let save = on_activate(
            button(
                format!("{row_key}:rename-save"),
                "check",
                strings.get("sessions.sidebar.session.rename.save"),
                palette,
            ),
            cx,
            |this, window, cx| this.save_rename(window, cx),
        )
        .test_support();
        let cancel = on_activate(
            button(
                format!("{row_key}:rename-cancel"),
                "close",
                strings.get("sessions.sidebar.session.rename.cancel"),
                palette,
            ),
            cx,
            |this, window, cx| this.cancel_rename(window, cx),
        )
        .test_support();
        let escape_input = input.clone();
        Some(
            div()
                .id(SharedString::from(format!("{row_key}:rename")))
                .accessibility_id(format!("{row_key}:rename"))
                .role(Role::Group)
                .aria_label(strings.get("sessions.sidebar.session.menu.rename"))
                .flex_1()
                .min_w_0()
                .h(px(20.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                // Escape cancels unless it ends an IME composition, which
                // the field handles. Either way it stops here, so it never
                // reaches the workspace's Stop.
                .capture_action(cx.listener(move |this, _: &Escape, window, cx| {
                    let composing = escape_input.update(cx, |input, cx| {
                        input.marked_text_range(window, cx).is_some()
                    });
                    if !composing {
                        cx.stop_propagation();
                        this.cancel_rename(window, cx);
                    }
                }))
                .on_action(|_: &Escape, _, cx| cx.stop_propagation())
                // A press outside the field saves, as an outside click does.
                .on_mouse_down_out(cx.listener(|this, _, window, cx| this.save_rename(window, cx)))
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&input)
                            .appearance(false)
                            .bordered(false)
                            .text_size(gpui_kit::rems(LABEL_SIZE)),
                    ),
                )
                .child(save)
                .child(cancel)
                .test_support()
                .into_any_element(),
        )
    }

    /// A status node keeping the newest session notification's text, also
    /// after its toast closes, so automation reads the outcome by a stable
    /// ID. The toast itself announces the text to screen readers.
    pub(crate) fn render_session_notice(&self) -> AnyElement {
        div()
            .id("session-notice")
            .role(Role::Status)
            .accessibility_id("session-notice")
            .when_some(self.session_management.notice.clone(), |this, notice| {
                this.aria_label(notice)
            })
            .absolute()
            .size_0()
            .overflow_hidden()
            .into_any_element()
    }

    /// Opens the archive or delete confirmation.
    pub(crate) fn open_removal_dialog(
        &mut self,
        confirm: RemovalConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session_management.confirm = Some(confirm);
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = workspace.upgrade() else {
                return dialog;
            };
            let state = this.read(cx);
            let Some(confirm) = state.session_management.confirm.clone() else {
                return dialog;
            };
            let never_ask = !state.shows_deletion_dialog();
            let palette = cx.global::<Palette>().clone();
            let strings = locale::strings(cx).clone();
            let direction = strings.direction();
            let title_key = if confirm.hard_delete {
                "sessions.sidebar.dialogs.deleteSession.title"
            } else {
                "sessions.sidebar.dialogs.archiveSession.title"
            };
            let action_key = if confirm.hard_delete {
                "sessions.sidebar.bulkActions.delete"
            } else {
                "sessions.sidebar.bulkActions.archive"
            };
            let on_cancel = workspace.clone();
            let on_confirm = workspace.clone();
            let on_toggle = workspace.clone();
            let on_close = workspace.clone();
            let text_button =
                |id: &'static str, label: SharedString, fill: Option<Hsla>, text: Hsla| {
                    div()
                        .id(id)
                        .role(Role::Button)
                        .accessibility_id(id)
                        .aria_label(label.clone())
                        .h(px(32.0))
                        .px(px(12.0))
                        .rounded(px(6.0))
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .text_color(text)
                        .when_some(fill, |this, fill| this.bg(fill))
                        .when(fill.is_none(), |this| {
                            this.border_1().border_color(palette.border)
                        })
                        .child(div().text_size(gpui_kit::rems(LABEL_SIZE)).child(label))
                        .test_support()
                };
            dialog
                .w(px(384.0))
                .close_button(false)
                .on_close(move |_, _, cx| {
                    let _ = on_close.update(cx, |this, cx| {
                        this.session_management.confirm = None;
                        cx.notify();
                    });
                })
                .title(strings.get(title_key))
                .child(
                    div()
                        .text_size(gpui_kit::rems(LABEL_SIZE))
                        .text_color(palette.muted_foreground)
                        .child(confirm.description(&strings)),
                )
                .footer(
                    div()
                        .w_full()
                        .flex_row_from(direction)
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .id("session-confirm-never-ask")
                                .role(Role::CheckBox)
                                .accessibility_id("session-confirm-never-ask")
                                .aria_label(strings.get("sessions.sidebar.dialogs.neverAsk"))
                                .aria_toggled(if never_ask {
                                    gpui_kit::Toggled::True
                                } else {
                                    gpui_kit::Toggled::False
                                })
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(6.0))
                                .cursor_pointer()
                                .text_color(palette.muted_foreground)
                                .child(glyph(
                                    if never_ask {
                                        "checkbox"
                                    } else {
                                        "checkbox-blank"
                                    },
                                    16.0,
                                    if never_ask {
                                        palette.primary
                                    } else {
                                        palette.muted_foreground
                                    },
                                ))
                                .child(
                                    div()
                                        .text_size(gpui_kit::rems(LABEL_SIZE))
                                        .child(strings.get("sessions.sidebar.dialogs.neverAsk")),
                                )
                                .on_click(move |_, window, cx| {
                                    let _ = on_toggle.update(cx, |this, cx| {
                                        this.toggle_deletion_dialog(window, cx)
                                    });
                                })
                                .test_support(),
                        )
                        .child(
                            div()
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    text_button(
                                        "session-confirm-cancel",
                                        strings.get("sessions.sidebar.dialogs.cancel"),
                                        None,
                                        palette.foreground,
                                    )
                                    .on_click(
                                        move |_, window, cx| {
                                            let _ = on_cancel.update(cx, |this, cx| {
                                                this.cancel_removal(window, cx)
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    text_button(
                                        "session-confirm",
                                        strings.get(action_key),
                                        Some(palette.status_error),
                                        palette.status_error_foreground,
                                    )
                                    .on_click(
                                        move |_, window, cx| {
                                            let _ = on_confirm.update(cx, |this, cx| {
                                                this.confirm_removal(window, cx)
                                            });
                                        },
                                    ),
                                ),
                        ),
                )
        });
        cx.notify();
    }
}

/// The minimum widths of `SubContent`: 220 px for worktrees, 180 px for
/// pins and folders.
fn submenu_min_width(kind: Submenu) -> f32 {
    match kind {
        Submenu::Worktree => 220.0,
        Submenu::Pin | Submenu::Folder => 180.0,
    }
}

fn first_enabled(entries: &[SubmenuEntry]) -> Option<usize> {
    entries.iter().position(SubmenuEntry::is_enabled)
}

fn menu_separator(palette: &Palette) -> AnyElement {
    div()
        .mx(px(-4.0))
        .my(px(2.0))
        .h(px(1.0))
        .bg(palette.border)
        .into_any_element()
}

/// What one line of the menu or a submenu shows.
pub(crate) struct MenuRow {
    pub(crate) id: String,
    pub(crate) label: SharedString,
    pub(crate) hint: Option<SharedString>,
    pub(crate) icon: Option<&'static str>,
    pub(crate) enabled: bool,
    pub(crate) highlighted: bool,
    pub(crate) destructive: bool,
    /// `Some` on a line that can carry the current choice's check.
    pub(crate) checked: Option<bool>,
    pub(crate) opens_submenu: bool,
}

/// One 28 px line: icon, label, then the check or the submenu chevron at
/// the end.
pub(crate) fn menu_row(
    row: MenuRow,
    palette: &Palette,
    direction: Direction,
    cx: &mut Context<Workspace>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (text, icon): (Hsla, Hsla) = if row.destructive {
        (palette.error, palette.error)
    } else {
        (palette.elevated_foreground, palette.muted_foreground)
    };
    let chevron = match direction {
        Direction::LeftToRight => "arrow-right-s",
        Direction::RightToLeft => "arrow-left-s",
    };
    div()
        .id(SharedString::from(row.id.clone()))
        .role(Role::MenuItem)
        .accessibility_id(row.id)
        .aria_label(row.label.clone())
        .when_some(row.hint.clone(), |this, hint| this.aria_description(hint))
        .when(row.opens_submenu, |this| {
            this.aria_expanded(row.highlighted)
        })
        .when_some(row.checked, |this, checked| {
            this.aria_toggled(if checked {
                gpui_kit::Toggled::True
            } else {
                gpui_kit::Toggled::False
            })
        })
        .when(!row.enabled, |this| {
            this.opacity(0.5)
                .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
        })
        .when_some(row.hint, |this, hint| {
            this.tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
                .tooltip_show_delay(HINT_DELAY)
        })
        .h(px(28.0))
        .px(px(8.0))
        .rounded(px(8.0))
        .flex_row_from(direction)
        .items_center()
        .gap(px(8.0))
        .when(row.enabled, |this| this.cursor_pointer())
        .when(row.highlighted, |this| {
            this.bg(if row.destructive {
                palette.error.opacity(0.1)
            } else {
                palette.hover
            })
        })
        .when_some(row.icon, |this, name| {
            this.child(
                div()
                    .margin_end(direction, px(4.0))
                    .child(glyph(name, 16.0, icon)),
            )
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                .text_color(text)
                .child(row.label),
        )
        .when(row.checked == Some(true), |this| {
            this.child(glyph("check", 14.0, palette.primary))
        })
        .when(row.opens_submenu, |this| {
            this.child(glyph(chevron, 16.0, palette.muted_foreground))
        })
}
