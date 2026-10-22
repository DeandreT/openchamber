//! The window layout: sidebar at the inline start, conversation in the center.

use gpui_kit::{
    Context, InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, Styled, Window,
    actions, canvas, div, prelude::FluentBuilder,
};

use crate::diagnostics;
use crate::locale::{self, Direction};
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let markers = self.take_frame_markers();
        let palette = cx.global::<Palette>().clone();
        let right_to_left = locale::strings(cx).direction() == Direction::RightToLeft;
        div()
            .id("workspace")
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
            .flex()
            .when(right_to_left, |this| this.flex_row_reverse())
            .bg(palette.background)
            .text_color(palette.foreground)
            .child(self.render_sidebar(&palette, right_to_left, cx))
            .child(
                div()
                    .id("conversation")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(palette.chat_background)
                    .child(self.render_header(&palette, cx))
                    .child(self.render_transcript(&palette, cx))
                    .child(self.render_composer(&palette, cx)),
            )
            .when(!markers.is_empty(), |this| {
                // A zero-size canvas paints after the views above it in the same frame.
                this.child(
                    canvas(
                        |_, _, _| {},
                        move |_, _, _, _| {
                            for (marker, started) in markers {
                                let elapsed = started.map(|started| started.elapsed());
                                diagnostics::mark(marker, elapsed, None);
                            }
                        },
                    )
                    .size_0(),
                )
            })
    }
}
