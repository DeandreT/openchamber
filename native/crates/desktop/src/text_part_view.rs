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
                .px_3()
                .py_2()
                .rounded(px(12.0))
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
