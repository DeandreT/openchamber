//! Reasoning parts.

use gpui_kit::{AnyElement, IntoElement, ParentElement, Styled, div, rems};

use crate::theme::Palette;
use crate::transcript_view::META_SIZE;

pub(crate) fn render(text: &str, palette: &Palette) -> AnyElement {
    div()
        .text_size(rems(META_SIZE))
        .italic()
        .text_color(palette.muted_foreground)
        .child(text.to_owned())
        .into_any_element()
}
