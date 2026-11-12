//! Text parts: the user's prompt bubble and an assistant's Markdown.

use gpui_kit::{
    AnyElement, App, IntoElement, ParentElement, Styled, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::PartId;
use openchamber_native_core::transcript::Message;

use crate::attachment_part_view;
use crate::locale::{self, Direction};
use crate::markdown_view::{self, Author};
use crate::part_state::PartState;
use crate::theme::Palette;
use crate::transcript_selection::TranscriptSelection;

/// The bubble in `ChatMessage.tsx`: `px-5 py-3` inside a 1 px border
/// (`border_1`) of the primary color at 5 %, `--radius-xl` corners and a
/// `--radius-sm` one at the bottom right, which the web sets as a physical
/// corner.
const BUBBLE_PADDING_X: f32 = 20.0;
const BUBBLE_PADDING_Y: f32 = 12.0;
const BUBBLE_BORDER_ALPHA: f32 = 0.05;
const BUBBLE_RADIUS: f32 = 12.0;
const BUBBLE_TAIL_RADIUS: f32 = 4.0;

/// The user's prompt in its end-aligned bubble.
pub(crate) fn user_message(
    message: &Message,
    parts: &PartState,
    palette: &Palette,
    direction: Direction,
    selection: &TranscriptSelection,
    cx: &App,
) -> AnyElement {
    let text = message.plain_text();
    // Attachments lead the bubble, above the text.
    let attachments =
        attachment_part_view::attachments(message, parts, palette, direction, locale::strings(cx));
    let has_text = !text.is_empty();
    let part_id = message
        .parts
        .first()
        .map(|part| part.id.as_str().to_owned())
        .unwrap_or_default();
    div()
        .flex()
        .justify_end()
        .child(
            div()
                .max_w(gpui_kit::relative(0.85))
                .px(px(BUBBLE_PADDING_X))
                .py(px(BUBBLE_PADDING_Y))
                .border_1()
                .border_color(palette.primary.opacity(BUBBLE_BORDER_ALPHA))
                .rounded(px(BUBBLE_RADIUS))
                .rounded_br(px(BUBBLE_TAIL_RADIUS))
                .bg(palette.user_message_background)
                .flex()
                .flex_col()
                .gap_2()
                .children(attachments)
                .when(has_text, |this| {
                    this.child(
                        selection
                            .view(&message.info.id, None, cx)
                            .map(|view| markdown_view::styled(view, palette, Author::User))
                            .unwrap_or_else(|| {
                                markdown_view::markdown(&part_id, &text, palette, Author::User)
                            }),
                    )
                }),
        )
        .into_any_element()
}

/// One assistant text part, from the retained selection view when it has one.
pub(crate) fn assistant_text(
    message: &Message,
    part: &PartId,
    text: &str,
    palette: &Palette,
    selection: &TranscriptSelection,
    cx: &App,
) -> AnyElement {
    selection
        .view(&message.info.id, Some(part), cx)
        .map(|view| markdown_view::styled(view, palette, Author::Assistant))
        .unwrap_or_else(|| markdown_view::markdown(part.as_str(), text, palette, Author::Assistant))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference's one-line bubble around a 154 px prompt on the 23 px
    /// body line measures 196 by 49.
    #[test]
    fn a_one_line_bubble_has_the_reference_box() {
        let border = 1.0;
        let width = 154.0 + 2.0 * (BUBBLE_PADDING_X + border);
        let height = 23.0 + 2.0 * (BUBBLE_PADDING_Y + border);
        assert_eq!((width, height), (196.0, 49.0));
    }
}
