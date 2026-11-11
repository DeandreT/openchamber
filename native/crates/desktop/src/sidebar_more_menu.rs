//! The sidebar toolbar's More menu (`SidebarHeader.tsx`, `morePageItems`):
//! the pages the sidebar opens in the main area, most used first. Each line
//! is a toggle that carries a check while its page is open.
//!
//! Usage and the integrations catalog have no native page yet; their lines
//! stay in place, reported as disabled. Overview and extension pages are not
//! listed. Each line keeps the control name its page had as a toolbar button,
//! so automation reaches a page by opening the menu first.

use gpui_kit::base::actions::{Cancel, Confirm, SelectDown, SelectUp};
use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Role,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, div, px,
};
use openchamber_native_core::session_actions::step_highlight_by;

use crate::activation::on_activate;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::session_menu::{MENU_CONTEXT, MenuRow, menu_row};
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The control name of the toolbar button that opens the menu.
pub(crate) const TRIGGER_ID: &str = "sidebar-more";

/// A page the menu opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    SourceBoard,
    ScheduledTasks,
    Archive,
    UsageStats,
    Integrations,
}

impl Page {
    fn id(self) -> &'static str {
        match self {
            Self::SourceBoard => "source-board",
            Self::ScheduledTasks => "scheduled-tasks",
            Self::Archive => "archive",
            Self::UsageStats => "usage-stats",
            Self::Integrations => "integrations",
        }
    }

    fn label_key(self) -> &'static str {
        match self {
            Self::SourceBoard => "sourceBoard.title",
            Self::ScheduledTasks => "sessions.sidebar.header.actions.scheduledTasks",
            Self::Archive => "sessions.sidebar.nav.archive",
            Self::UsageStats => "usageStats.openAction",
            Self::Integrations => "integrationsCatalog.sidebar.open",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::SourceBoard => "git-pull-request",
            Self::ScheduledTasks => "calendar-schedule",
            Self::Archive => "archive",
            Self::UsageStats => "bar-chart",
            Self::Integrations => "store-2",
        }
    }

    /// Whether this client has the page.
    fn available(self) -> bool {
        !matches!(self, Self::UsageStats | Self::Integrations)
    }
}

/// The menu's lines. The bundled catalog offers outside services, so
/// enterprise mode, which waits for administrator-curated sources, lists no
/// integrations line.
fn pages(enterprise: bool) -> Vec<Page> {
    let mut pages = vec![
        Page::SourceBoard,
        Page::ScheduledTasks,
        Page::Archive,
        Page::UsageStats,
    ];
    if !enterprise {
        pages.push(Page::Integrations);
    }
    pages
}

/// The open menu.
pub(crate) struct MoreMenu {
    highlighted: Option<usize>,
    focus: gpui_kit::FocusHandle,
}

impl Workspace {
    fn more_pages(&self) -> Vec<Page> {
        pages(self.enterprise.enterprise_mode())
    }

    fn page_open(&self, page: Page) -> bool {
        match page {
            Page::SourceBoard => self.source_board_open,
            Page::ScheduledTasks => self.scheduled_tasks_open,
            Page::Archive => self.archive_open,
            Page::UsageStats | Page::Integrations => false,
        }
    }

    /// Whether a page from the menu is on screen, which lights its trigger.
    pub(crate) fn more_page_open(&self) -> bool {
        self.more_pages()
            .into_iter()
            .any(|page| self.page_open(page))
    }

    /// The toolbar's More button.
    pub(crate) fn toggle_more_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_management.more_menu.take().is_some() {
            cx.notify();
            return;
        }
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.session_management.more_menu = Some(MoreMenu {
            highlighted: None,
            focus,
        });
        cx.notify();
    }

    /// Closes the menu, then opens or closes `page`.
    fn toggle_more_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        if !page.available() {
            return;
        }
        self.session_management.more_menu = None;
        match page {
            Page::SourceBoard => self.toggle_source_board(window, cx),
            Page::ScheduledTasks => self.toggle_scheduled_tasks(window, cx),
            Page::Archive => self.toggle_archive_page(window, cx),
            Page::UsageStats | Page::Integrations => {}
        }
        cx.notify();
    }

    fn step_more_menu(&mut self, step: isize, cx: &mut Context<Self>) {
        let pages = self.more_pages();
        let Some(menu) = self.session_management.more_menu.as_mut() else {
            return;
        };
        menu.highlighted = step_highlight_by(
            pages.len(),
            |index| pages[index].available(),
            menu.highlighted,
            step,
        );
        cx.notify();
    }

    fn confirm_more_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = self
            .session_management
            .more_menu
            .as_ref()
            .and_then(|menu| menu.highlighted)
            .and_then(|index| self.more_pages().get(index).copied());
        if let Some(page) = page {
            self.toggle_more_page(page, window, cx);
        }
    }

    /// The open menu, below the More button and aligned to its start.
    pub(crate) fn render_more_menu(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.session_management.more_menu.as_ref()?;
        let strings = locale::strings(cx).clone();
        let mut rows: Vec<AnyElement> = Vec::new();
        for (index, page) in self.more_pages().into_iter().enumerate() {
            let enabled = page.available();
            let row = menu_row(
                MenuRow {
                    id: page.id().to_owned(),
                    label: strings.get(page.label_key()),
                    hint: None,
                    icon: Some(page.icon()),
                    enabled,
                    highlighted: enabled && menu.highlighted == Some(index),
                    destructive: false,
                    checked: Some(self.page_open(page)),
                    opens_submenu: false,
                },
                palette,
                direction,
                cx,
            )
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if let Some(menu) = this.session_management.more_menu.as_mut()
                    && enabled
                    && menu.highlighted != Some(index)
                {
                    menu.highlighted = Some(index);
                    cx.notify();
                }
            }));
            rows.push(if enabled {
                on_activate(row, cx, move |this, window, cx| {
                    this.toggle_more_page(page, window, cx)
                })
                .test_support()
                .into_any_element()
            } else {
                row.test_support().into_any_element()
            });
        }
        let popup = div()
            .id("sidebar-more-menu")
            .role(Role::Menu)
            .accessibility_id("sidebar-more-menu")
            .aria_label(strings.get("sessions.sidebar.header.actions.more"))
            .track_focus(&menu.focus)
            .key_context(MENU_CONTEXT)
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step_more_menu(1, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step_more_menu(-1, cx)))
            .on_action(
                cx.listener(|this, _: &Confirm, window, cx| this.confirm_more_menu(window, cx)),
            )
            .on_action(cx.listener(|this, _: &Cancel, _, cx| {
                this.session_management.more_menu = None;
                cx.notify();
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.session_management.more_menu = None;
                cx.notify();
            }))
            .occlude()
            .min_w(px(200.0))
            .p(px(4.0))
            .rounded(px(12.0))
            .bg(palette.elevated)
            .shadow(crate::elevation::floating(palette))
            .flex()
            .flex_col()
            .children(rows)
            .test_support();
        let anchor = match direction {
            Direction::LeftToRight => gpui_kit::Anchor::TopLeft,
            Direction::RightToLeft => gpui_kit::Anchor::TopRight,
        };
        Some(
            div()
                .absolute()
                .top(px(24.0))
                .inset_start(direction, px(0.0))
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
}

#[cfg(test)]
mod tests {
    use super::{Page, pages};

    #[test]
    fn enterprise_mode_lists_no_integrations_line() {
        assert_eq!(
            pages(false),
            [
                Page::SourceBoard,
                Page::ScheduledTasks,
                Page::Archive,
                Page::UsageStats,
                Page::Integrations
            ]
        );
        assert!(!pages(true).contains(&Page::Integrations));
    }

    #[test]
    fn pages_without_a_native_surface_cannot_be_opened() {
        let unavailable: Vec<&str> = pages(false)
            .into_iter()
            .filter(|page| !page.available())
            .map(Page::id)
            .collect();
        assert_eq!(unavailable, ["usage-stats", "integrations"]);
    }
}
