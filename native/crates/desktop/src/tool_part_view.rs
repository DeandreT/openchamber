//! Tool call parts.

use gpui_kit::{
    AnyElement, FontWeight, IntoElement, ParentElement, Styled, div, prelude::FluentBuilder, rems,
};

use crate::chrome::glyph;
use crate::theme::Palette;
use crate::transcript_view::META_SIZE;

/// One tool call; `controls` are the subagent row's Open and Stop actions.
pub(crate) fn render(
    tool: &str,
    title: Option<&str>,
    output: Option<&str>,
    controls: Option<AnyElement>,
    palette: &Palette,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_size(rems(META_SIZE))
        .text_color(palette.muted_foreground)
        .child(glyph("terminal", 16.0, palette.muted_foreground))
        .child(div().font_weight(FontWeight::MEDIUM).child(tool.to_owned()))
        .when_some(title, |this, title| {
            this.child(div().truncate().child(title.to_owned()))
        })
        .when_some(output, |this, output| this.child(output.to_owned()))
        .children(controls)
        .into_any_element()
}
