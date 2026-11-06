//! File and agent-mention parts.

use gpui_kit::{AnyElement, IntoElement, ParentElement, Styled, div, rems};

use crate::theme::Palette;
use crate::transcript_view::META_SIZE;

pub(crate) fn file(filename: &str, palette: &Palette) -> AnyElement {
    div()
        .text_size(rems(META_SIZE))
        .text_color(palette.muted_foreground)
        .child(filename.to_owned())
        .into_any_element()
}

pub(crate) fn agent_mention(name: &str, palette: &Palette) -> AnyElement {
    div()
        .text_size(rems(META_SIZE))
        .text_color(palette.muted_foreground)
        .child(format!("@{name}"))
        .into_any_element()
}
