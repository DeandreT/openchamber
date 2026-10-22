//! The window layout of the frameless desktop window: the sidebar column at
//! the inline start; the main column with its header, the chat, the work
//! status panel and the panel rail.

use gpui_kit::{
    Context, InteractiveElement, IntoElement, KeyBinding, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, actions, canvas, div, prelude::FluentBuilder, px,
};

use crate::chrome::{self, ButtonTone, IconButton, TITLE_ROW_HEIGHT};
use crate::diagnostics;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

actions!(
    openchamber,
    [
        /// Selects the next session row.
        SelectNextSession,
        /// Selects the previous session row.
        SelectPreviousSession,
        /// Stops the selected session's response.
        StopGenerating,
    ]
);

const KEY_CONTEXT: &str = "OpenChamberWorkspace";

/// Registers the workspace's keyboard shortcuts.
pub fn bind_keys(cx: &mut gpui_kit::App) {
    cx.bind_keys([
        KeyBinding::new("alt-down", SelectNextSession, Some(KEY_CONTEXT)),
        KeyBinding::new("alt-up", SelectPreviousSession, Some(KEY_CONTEXT)),
        KeyBinding::new("escape", StopGenerating, Some(KEY_CONTEXT)),
    ]);
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let markers = self.take_frame_markers();
        let palette = cx.global::<Palette>().clone();
        let direction = locale::strings(cx).direction();
        let chat = if self.selected.is_some() {
            div()
                .id("conversation")
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .bg(palette.chat_background)
                .child(self.render_transcript(&palette, cx))
                .child(self.render_composer(&palette, direction, cx))
                .into_any_element()
        } else {
            self.render_startup(&palette, direction, window, cx)
                .into_any_element()
        };
        div()
            .id("workspace")
            .role(gpui_kit::Role::Group)
            .accessibility_id("workspace")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &SelectNextSession, window, cx| {
                this.select_adjacent(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousSession, window, cx| {
                this.select_adjacent(-1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &StopGenerating, _, cx| this.stop(cx)))
            .size_full()
            .flex_row_from(direction)
            .bg(palette.background)
            .text_color(palette.foreground)
            .when(self.sidebar_open, |this| {
                this.child(self.render_sidebar(&palette, direction, cx))
            })
            .child(
                div()
                    .id("main")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(palette.background)
                    .child(self.render_header(&palette, direction, window, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex_row_from(direction)
                            .child(chat)
                            .when(self.work_status_open, |this| {
                                this.child(self.render_work_status_panel(&palette, direction, cx))
                            })
                            .child(self.render_rail(&palette, direction, cx)),
                    ),
            )
            .when(!markers.is_empty(), |this| {
                // A zero-size canvas paints after the views above it in the same frame.
                this.child(
                    canvas(
                        |_, _, _| {},
                        move |_, _, _, _| {
                            for (marker, started, count) in markers {
                                let elapsed = started.map(|started| started.elapsed());
                                diagnostics::mark(marker, elapsed, count);
                            }
                        },
                    )
                    .size_0(),
                )
            })
    }
}

impl Workspace {
    /// The main column's title row. Window controls keep their physical
    /// order and the right edge in every language, as in the web client.
    fn render_header(
        &self,
        palette: &Palette,
        direction: Direction,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        div()
            .id("header")
            .h(px(TITLE_ROW_HEIGHT))
            .flex_shrink_0()
            .flex()
            .bg(palette.background)
            .child(
                chrome::drag_region("header-drag", cx)
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex_row_from(direction)
                    .items_center()
                    .when(!self.sidebar_open, |this| {
                        this.padding_start(direction, px(12.0))
                            .child(self.render_title_controls(palette, direction, cx))
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                IconButton::new(
                                    "mini-chat",
                                    "picture-in-picture-2",
                                    strings.get("header.actions.newMiniChatAria"),
                                )
                                .render(palette, cx, |_, _, _| {}),
                            )
                            .child(
                                IconButton::new(
                                    "work-status-toggle",
                                    "list-indefinite",
                                    strings.get("header.workStatusPanel.toggleAria"),
                                )
                                .tone(if self.work_status_open {
                                    ButtonTone::Active
                                } else {
                                    ButtonTone::Quiet
                                })
                                .render(
                                    palette,
                                    cx,
                                    |this, _, cx| this.toggle_work_status(cx),
                                ),
                            ),
                    )
                    .child(div().w(px(12.0)).flex_shrink_0()),
            )
            .child(chrome::window_controls(window, &strings, palette, cx))
    }
}
