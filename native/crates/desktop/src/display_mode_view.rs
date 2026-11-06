//! The toolbar's Display mode menu (`SidebarHeader.tsx`): the Sessions
//! grouping as a submenu that names the current choice, then Collapse all
//! and Expand all. Choosing a grouping keeps the menu open, as the web's
//! view settings do, and saves `sidebarGrouping`; the list follows at once.
//!
//! The sections manager, Select sessions, project sorting and display,
//! Detailed rows, Name worktrees by branch and Mark all read belong to
//! features the native sidebar does not have yet and are not listed.

use gpui_kit::base::actions::{Cancel, Confirm, SelectDown, SelectLeft, SelectRight, SelectUp};
use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Role,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, div, px,
};
use openchamber_native_core::sidebar_rows::{ActivityKey, SidebarViewMode};
use openchamber_native_transport::client::SettingsChanges;

use crate::activation::on_activate;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::session_menu::{MenuRow, menu_row};
use crate::sidebar_item::SidebarItem;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// A line of the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Line {
    Grouping,
    CollapseAll,
    ExpandAll,
}

const LINES: [Line; 3] = [Line::Grouping, Line::CollapseAll, Line::ExpandAll];
const GROUPINGS: [SidebarViewMode; 2] = [SidebarViewMode::Works, SidebarViewMode::WorksByTime];

fn grouping_label_key(mode: SidebarViewMode) -> &'static str {
    match mode {
        SidebarViewMode::WorksByTime | SidebarViewMode::Timeline => {
            "sessions.sidebar.header.listGrouping.time"
        }
        SidebarViewMode::Works | SidebarViewMode::Projects => {
            "sessions.sidebar.header.listGrouping.project"
        }
    }
}

fn grouping_setting(mode: SidebarViewMode) -> &'static str {
    match mode {
        SidebarViewMode::WorksByTime | SidebarViewMode::Timeline => "time",
        SidebarViewMode::Works | SidebarViewMode::Projects => "project",
    }
}

/// The open menu.
pub(crate) struct DisplayMenu {
    highlighted: Option<usize>,
    /// The grouping submenu, with its highlighted choice when the keyboard
    /// is in it.
    grouping: Option<Option<usize>>,
    focus: gpui_kit::FocusHandle,
}

impl Workspace {
    fn sidebar_grouping(&self) -> SidebarViewMode {
        self.send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.sidebar_view)
            .unwrap_or_default()
    }

    /// The toolbar's Display mode button.
    pub(crate) fn toggle_display_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_management.display_menu.take().is_some() {
            cx.notify();
            return;
        }
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.session_management.display_menu = Some(DisplayMenu {
            highlighted: None,
            grouping: None,
            focus,
        });
        cx.notify();
    }

    /// Lists sessions by project or by time. The choice shows at once and is
    /// saved for every window of this server.
    fn choose_grouping(
        &mut self,
        mode: SidebarViewMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar_grouping() == mode {
            return;
        }
        if let Some(settings) = self.send_catalog.settings.as_mut() {
            settings.sidebar_view = Some(mode);
        }
        self.rebuild_sidebar();
        let grouping = grouping_setting(mode);
        self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    client
                        .save_settings(&SettingsChanges {
                            sidebar_grouping: Some(grouping),
                            ..SettingsChanges::default()
                        })
                        .await
                })
            },
            |_, result, _, _| {
                if let Some(Err(error)) = result {
                    tracing::warn!(%error, "the sidebar grouping could not be saved");
                }
            },
            window,
            cx,
        );
        cx.notify();
    }

    /// Collapse all: every zone and project folds, and every worktree
    /// closes its sessions.
    pub(crate) fn collapse_all(&mut self, cx: &mut Context<Self>) {
        let projects: Vec<_> = self
            .topology
            .projects
            .iter()
            .map(|project| project.id.clone())
            .collect();
        let state = &mut self.sidebar_state;
        state.collapsed_groups.clear();
        state.expanded_works.clear();
        state.collapsed_activities.extend([
            ActivityKey::Chats,
            ActivityKey::Pinned,
            ActivityKey::Work,
            ActivityKey::Timeline,
        ]);
        state.collapsed_projects.extend(projects);
        self.rebuild_sidebar();
        cx.notify();
    }

    /// Expand all: every zone and project opens, and every worktree lists
    /// its sessions.
    pub(crate) fn expand_all(&mut self, cx: &mut Context<Self>) {
        let state = &mut self.sidebar_state;
        state.collapsed_groups.clear();
        state.collapsed_activities.clear();
        state.collapsed_projects.clear();
        self.rebuild_sidebar();
        let works: Vec<String> = self
            .sidebar_items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::Worktree { group_key, .. } => Some(group_key.clone()),
                _ => None,
            })
            .collect();
        self.sidebar_state.expanded_works.extend(works);
        self.rebuild_sidebar();
        cx.notify();
    }

    fn run_display_line(&mut self, line: Line, keyboard: bool, cx: &mut Context<Self>) {
        match line {
            Line::Grouping => {
                if let Some(menu) = self.session_management.display_menu.as_mut() {
                    menu.highlighted = Some(0);
                    menu.grouping = Some(keyboard.then_some(0));
                }
                cx.notify();
            }
            Line::CollapseAll => {
                self.session_management.display_menu = None;
                self.collapse_all(cx);
            }
            Line::ExpandAll => {
                self.session_management.display_menu = None;
                self.expand_all(cx);
            }
        }
    }

    fn step_display_menu(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(menu) = self.session_management.display_menu.as_mut() else {
            return;
        };
        let wrap = |from: Option<usize>, count: usize| {
            openchamber_native_core::session_actions::step_highlight_by(count, |_| true, from, step)
        };
        match &mut menu.grouping {
            Some(choice @ Some(_)) => *choice = wrap(*choice, GROUPINGS.len()),
            _ => {
                menu.grouping = None;
                menu.highlighted = wrap(menu.highlighted, LINES.len());
            }
        }
        cx.notify();
    }

    fn confirm_display_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.session_management.display_menu.as_ref() else {
            return;
        };
        if let Some(Some(choice)) = menu.grouping {
            self.choose_grouping(GROUPINGS[choice], window, cx);
            return;
        }
        if let Some(line) = menu.highlighted.and_then(|index| LINES.get(index).copied()) {
            self.run_display_line(line, true, cx);
        }
    }

    fn horizontal_display_menu(&mut self, toward_end: bool, cx: &mut Context<Self>) {
        let Some(menu) = self.session_management.display_menu.as_mut() else {
            return;
        };
        if toward_end && menu.highlighted == Some(0) {
            menu.grouping = Some(Some(0));
        } else if !toward_end {
            menu.grouping = None;
        }
        cx.notify();
    }

    /// The open menu, below the Display mode button's end.
    pub(crate) fn render_display_menu(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.session_management.display_menu.as_ref()?;
        let strings = locale::strings(cx).clone();
        let current = self.sidebar_grouping();
        let grouping_open = menu.grouping.is_some();
        let lines = LINES
            .iter()
            .enumerate()
            .map(|(index, line)| {
                let line = *line;
                let (id, label, icon) = match line {
                    Line::Grouping => (
                        "grouping",
                        strings.get("sessions.sidebar.header.listGrouping.label"),
                        if current == SidebarViewMode::WorksByTime {
                            "time"
                        } else {
                            "folders"
                        },
                    ),
                    Line::CollapseAll => (
                        "collapse-all",
                        strings.get("sessions.sidebar.header.displayMode.collapseAll"),
                        "contract-up-down",
                    ),
                    Line::ExpandAll => (
                        "expand-all",
                        strings.get("sessions.sidebar.header.displayMode.expandAll"),
                        "expand-up-down",
                    ),
                };
                let row = menu_row(
                    MenuRow {
                        id: format!("display-menu-item:{id}"),
                        label,
                        // The grouping names its current choice.
                        hint: (line == Line::Grouping)
                            .then(|| strings.get(grouping_label_key(current))),
                        icon: Some(icon),
                        enabled: true,
                        highlighted: menu.highlighted == Some(index),
                        destructive: false,
                        checked: None,
                        opens_submenu: line == Line::Grouping,
                    },
                    palette,
                    direction,
                    cx,
                )
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if let Some(menu) = this.session_management.display_menu.as_mut()
                        && menu.highlighted != Some(index)
                    {
                        menu.highlighted = Some(index);
                        menu.grouping = (line == Line::Grouping).then_some(None);
                        cx.notify();
                    }
                }));
                let row = on_activate(row, cx, move |this, _, cx| {
                    this.run_display_line(line, false, cx)
                })
                .test_support();
                let submenu = (line == Line::Grouping && grouping_open)
                    .then(|| self.render_grouping_submenu(menu, current, palette, direction, cx));
                div()
                    .relative()
                    .child(row)
                    .children(submenu)
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let popup = div()
            .id("display-menu")
            .role(Role::Menu)
            .accessibility_id("display-menu")
            .aria_label(strings.get("sessions.sidebar.header.displayMode.label"))
            .track_focus(&menu.focus)
            .key_context(crate::session_menu::MENU_CONTEXT)
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step_display_menu(1, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step_display_menu(-1, cx)))
            .on_action(cx.listener(move |this, _: &SelectRight, _, cx| {
                this.horizontal_display_menu(direction == Direction::LeftToRight, cx)
            }))
            .on_action(cx.listener(move |this, _: &SelectLeft, _, cx| {
                this.horizontal_display_menu(direction == Direction::RightToLeft, cx)
            }))
            .on_action(
                cx.listener(|this, _: &Confirm, window, cx| this.confirm_display_menu(window, cx)),
            )
            .on_action(cx.listener(|this, _: &Cancel, _, cx| {
                this.session_management.display_menu = None;
                cx.notify();
            }))
            .on_mouse_down_out(
                cx.listener(|this, event: &gpui_kit::MouseDownEvent, _, cx| {
                    let inside = this
                        .session_management
                        .submenu_bounds
                        .is_some_and(|bounds| bounds.contains(&event.position));
                    if !inside {
                        this.session_management.display_menu = None;
                        cx.notify();
                    }
                }),
            )
            .occlude()
            .min_w(px(200.0))
            .p(px(4.0))
            .rounded(px(12.0))
            .bg(palette.elevated)
            .shadow(crate::elevation::floating(palette))
            .flex()
            .flex_col()
            .children(lines)
            .test_support();
        let anchor = match direction {
            Direction::LeftToRight => gpui_kit::Anchor::TopRight,
            Direction::RightToLeft => gpui_kit::Anchor::TopLeft,
        };
        Some(
            div()
                .absolute()
                .top(px(24.0))
                .inset_end(direction, px(0.0))
                .size_0()
                .child(
                    gpui_kit::deferred(
                        gpui_kit::anchored()
                            .anchor(anchor)
                            .snap_to_window_with_margin(gpui_kit::Edges::all(px(8.0)))
                            .child(div().mt(px(4.0)).child(popup)),
                    )
                    .with_priority(1),
                )
                .into_any_element(),
        )
    }

    fn render_grouping_submenu(
        &self,
        menu: &DisplayMenu,
        current: SidebarViewMode,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let choices = GROUPINGS
            .iter()
            .enumerate()
            .map(|(index, mode)| {
                let mode = *mode;
                let row = menu_row(
                    MenuRow {
                        id: format!("display-menu-grouping:{}", grouping_setting(mode)),
                        label: strings.get(grouping_label_key(mode)),
                        hint: None,
                        icon: None,
                        enabled: true,
                        highlighted: menu.grouping == Some(Some(index)),
                        destructive: false,
                        checked: Some(current == mode),
                        opens_submenu: false,
                    },
                    palette,
                    direction,
                    cx,
                )
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if let Some(menu) = this.session_management.display_menu.as_mut()
                        && menu.grouping != Some(Some(index))
                    {
                        menu.grouping = Some(Some(index));
                        cx.notify();
                    }
                }));
                on_activate(row, cx, move |this, window, cx| {
                    this.choose_grouping(mode, window, cx)
                })
                .test_support()
                .into_any_element()
            })
            .collect::<Vec<_>>();
        let workspace = cx.entity().downgrade();
        let panel = div()
            .id("display-menu-grouping")
            .role(Role::Menu)
            .accessibility_id("display-menu-grouping")
            .occlude()
            .min_w(px(160.0))
            .p(px(4.0))
            .rounded(px(12.0))
            .bg(palette.elevated)
            .shadow(crate::elevation::floating(palette))
            .flex()
            .flex_col()
            .children(choices)
            .child(
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
            Direction::LeftToRight => gpui_kit::Anchor::TopLeft,
            Direction::RightToLeft => gpui_kit::Anchor::TopRight,
        };
        div()
            .absolute()
            .top(px(-4.0))
            .inset_end(direction, px(0.0))
            .size_0()
            .child(
                gpui_kit::deferred(
                    gpui_kit::anchored()
                        .anchor(anchor)
                        .snap_to_window_with_margin(gpui_kit::Edges::all(px(8.0)))
                        .child(div().margin_start(direction, px(8.0)).child(panel)),
                )
                .with_priority(2),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grouping_saves_the_web_setting_values() {
        assert_eq!(grouping_setting(SidebarViewMode::Works), "project");
        assert_eq!(grouping_setting(SidebarViewMode::WorksByTime), "time");
        assert_eq!(
            grouping_label_key(SidebarViewMode::WorksByTime),
            "sessions.sidebar.header.listGrouping.time"
        );
    }
}
