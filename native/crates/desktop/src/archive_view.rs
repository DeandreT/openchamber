//! The archive page: archived sessions with Restore and Delete.
//!
//! A reduced port of `ArchiveView.tsx` that replaces the chat area while it
//! is open. It lists the sessions `isRestorableOnItsOwn` admits, most
//! recently archived first; a subsession under an archived parent comes back
//! with that parent and is not listed.

use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::session_actions;
use openchamber_native_core::session_index::IndexStatus;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

const LABEL_SIZE: f32 = 0.843_75;
const LABEL_LINE: f32 = 1.25;
const MICRO_SIZE: f32 = 0.75;
const MICRO_LINE: f32 = 1.0;
const PAGE_WIDTH: f32 = 768.0;

impl Workspace {
    /// The toolbar's Archive button opens the page and closes it again.
    pub(crate) fn toggle_archive_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.archive_open = !self.archive_open;
        if !self.archive_open {
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn render_archive_page(
        &self,
        palette: &Palette,
        direction: Direction,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let sessions = session_actions::archive_page(&self.sessions);
        let count = sessions.len();
        let count_label = if count == 1 {
            strings.format(
                "sessions.archivePage.countSingle",
                &[("count", &count.to_string())],
            )
        } else {
            strings.format(
                "sessions.archivePage.countPlural",
                &[("count", &count.to_string())],
            )
        };
        // Empty means no archived sessions only once the list has loaded.
        let empty_label = match self.sessions.status() {
            IndexStatus::Failed if !self.sessions.has_loaded() => {
                strings.get("sessions.sidebar.group.empty.loadFailed")
            }
            _ if !self.sessions.has_loaded() && !self.is_synthetic() => {
                strings.get("sessions.sidebar.group.empty.loadingSessions")
            }
            _ => strings.get("sessions.archivePage.empty.noArchived"),
        };
        let untitled = strings.get("sessions.sidebar.session.untitled");
        let rows: Vec<AnyElement> = sessions
            .iter()
            .map(|session| {
                let id = session.id.clone();
                let title: SharedString = if session.title.is_empty() {
                    untitled.clone()
                } else {
                    session.title.clone().into()
                };
                let row_key = format!("archive-row:{id}");
                let group = SharedString::from(format!("{row_key}:hover"));
                let action =
                    |name: &str, icon: &'static str, label: SharedString, hover: gpui_kit::Hsla| {
                        let key = format!("archive-{name}:{id}");
                        div()
                            .id(SharedString::from(key.clone()))
                            .group(SharedString::from(key.clone()))
                            .role(Role::Button)
                            .accessibility_id(key.clone())
                            .aria_label(label)
                            .size(px(24.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .opacity(0.0)
                            .group_hover(group.clone(), |style| style.opacity(1.0))
                            .focus(|style| style.opacity(1.0))
                            .child(
                                glyph(icon, 14.0, palette.muted_foreground)
                                    .group_hover(SharedString::from(key), move |style| {
                                        style.text_color(hover)
                                    }),
                            )
                    };
                let restore_id = id.clone();
                let delete_id = id.clone();
                let restore = on_activate(
                    action(
                        "restore",
                        "inbox-unarchive",
                        strings.format(
                            "sessions.archivePage.restoreSessionAria",
                            &[("title", &title)],
                        ),
                        palette.foreground,
                    ),
                    cx,
                    move |this, window, cx| {
                        this.restore_sessions(vec![restore_id.clone()], None, false, window, cx);
                    },
                )
                .test_support();
                let delete = on_activate(
                    action(
                        "delete",
                        "delete-bin",
                        strings.format(
                            "sessions.archivePage.deleteSessionAria",
                            &[("title", &title)],
                        ),
                        palette.error,
                    ),
                    cx,
                    move |this, window, cx| this.request_removal(&delete_id, true, window, cx),
                )
                .test_support();
                div()
                    .id(SharedString::from(row_key.clone()))
                    .role(Role::ListItem)
                    .accessibility_id(row_key)
                    .aria_label(title.clone())
                    .group(group)
                    .h(px(28.0))
                    .padding_start(
                        direction,
                        px(if session.parent_id.is_some() {
                            24.0
                        } else {
                            8.0
                        }),
                    )
                    .padding_end(direction, px(4.0))
                    .rounded(px(6.0))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(4.0))
                    .hover(|style| style.bg(palette.hover.opacity(0.4)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .css_text(LABEL_SIZE, LABEL_LINE, cx)
                            .text_color(palette.foreground)
                            .child(title),
                    )
                    .child(restore)
                    .child(delete)
                    .test_support()
                    .into_any_element()
            })
            .collect();
        div()
            .id("archive-page")
            .role(Role::Region)
            .accessibility_id("archive-page")
            .aria_label(strings.get("sessions.archivePage.title"))
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(palette.background)
            .overflow_y_scroll()
            .px(px(24.0))
            .py(px(12.0))
            .child(
                div()
                    .mx_auto()
                    .w_full()
                    .max_w(px(PAGE_WIDTH))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex_row_from(direction)
                            .justify_end()
                            .h(px(32.0))
                            .items_center()
                            .child(
                                div()
                                    .id("archive-count")
                                    .role(Role::Status)
                                    .accessibility_id("archive-count")
                                    .aria_label(count_label.clone())
                                    .css_text(MICRO_SIZE, MICRO_LINE, cx)
                                    .text_color(palette.muted_foreground)
                                    .child(count_label),
                            ),
                    )
                    .when(rows.is_empty(), |this| {
                        this.child(
                            div()
                                .py(px(40.0))
                                .text_center()
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                                .text_color(palette.muted_foreground)
                                .child(empty_label),
                        )
                    })
                    .children(rows),
            )
            .into_any_element()
    }
}
