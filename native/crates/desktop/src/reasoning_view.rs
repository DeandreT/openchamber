//! Reasoning blocks and the rows that borrow their look (`ReasoningPart.tsx`):
//! a compaction summary.
//!
//! Folded, a block is one tool-style line: its icon, which turns into a
//! chevron under the pointer, its title, and a one-line summary once the
//! text stopped streaming. Opened, the body is the text as Markdown in a
//! capped scroll box beside the block line.

use gpui_kit::base::text::TextView;
use gpui_kit::{
    AnyElement, ElementId, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, div, prelude::FluentBuilder,
    px, rems,
};

use crate::activation::on_activate;
use crate::chrome::glyph;
use crate::locale::Direction;
use crate::logical::LogicalStyledExt;
use crate::markdown_view::{self, Author};
use crate::theme::Palette;
use crate::transcript_view::META_SIZE;
use crate::workspace::Workspace;

/// The reasoning box's `max-h-80`.
pub(crate) const REASONING_MAX_HEIGHT: f32 = 320.0;
/// A tool row's line height: `leading-6` at the desktop breakpoint.
pub(crate) const ROW_LINE_HEIGHT: f32 = 24.0;

/// What a block shows besides its text.
pub(crate) struct BlockLook {
    pub(crate) icon: &'static str,
    /// The icon's color when it is not the tools' own (a failed compaction).
    pub(crate) icon_color: Option<Hsla>,
    pub(crate) title: SharedString,
    pub(crate) expand_label: SharedString,
    pub(crate) collapse_label: SharedString,
    pub(crate) max_height: f32,
    /// Assistant prose for a compaction summary; muted meta text for reasoning.
    pub(crate) prose: bool,
}

/// One block's state for a frame.
pub(crate) struct Block<'a> {
    /// Unique among the transcript's elements.
    pub(crate) id: SharedString,
    pub(crate) text: &'a str,
    /// The folded line's preview; empty while streaming.
    pub(crate) summary: SharedString,
    pub(crate) streaming: bool,
    pub(crate) expanded: bool,
    /// Which of the busy dots show this second.
    pub(crate) dots: [bool; 3],
}

/// `BusyDots`: the dots fill in one by one, then empty from the first, one
/// step a second.
pub(crate) fn busy_dots(second: i64) -> [bool; 3] {
    const PHASES: [[bool; 3]; 5] = [
        [true, false, false],
        [true, true, false],
        [true, true, true],
        [false, true, true],
        [false, false, true],
    ];
    PHASES[second.rem_euclid(PHASES.len() as i64) as usize]
}

pub(crate) fn render(
    block: Block<'_>,
    look: BlockLook,
    palette: &Palette,
    direction: Direction,
    cx: &gpui_kit::Context<Workspace>,
    toggle: impl Fn(&mut Workspace, &mut gpui_kit::Window, &mut gpui_kit::Context<Workspace>)
    + Clone
    + 'static,
) -> AnyElement {
    let Block {
        id,
        text,
        summary,
        streaming,
        expanded,
        dots,
    } = block;
    let group: SharedString = format!("block:{id}").into();
    let header = on_activate(
        div()
            .id(ElementId::Name(format!("{id}:toggle").into()))
            .role(Role::Button)
            .accessibility_id(format!("{id}:toggle"))
            .aria_label(if expanded {
                look.collapse_label.clone()
            } else {
                look.expand_label.clone()
            })
            .aria_expanded(expanded)
            .group(group.clone())
            .flex()
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .padding_start(direction, px(1.0))
            .padding_end(direction, px(8.0))
            .py(px(6.0))
            .rounded(px(12.0))
            .cursor_pointer()
            .child(toggle_icon(
                &group,
                look.icon,
                look.icon_color.unwrap_or(palette.tools_icon),
                expanded,
                palette,
            ))
            .child(
                div()
                    .flex()
                    .flex_row_from(direction)
                    .flex_shrink_0()
                    .items_center()
                    .text_size(rems(META_SIZE))
                    .line_height(px(ROW_LINE_HEIGHT))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(palette.tools_title)
                    .child(look.title.clone())
                    .when(streaming, |this| this.child(dots_element(dots, direction))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(rems(META_SIZE))
                    .line_height(px(ROW_LINE_HEIGHT))
                    .text_color(palette.tools_description)
                    .when(!streaming && !expanded, |this| this.child(summary)),
            )
            .test_support(),
        cx,
        toggle.clone(),
    );
    div()
        .flex()
        .flex_col()
        .child(header)
        .when(expanded, |this| {
            this.child(block_body(
                format!("{id}:line").into(),
                palette,
                direction,
                cx,
                toggle,
                div()
                    .id(ElementId::Name(format!("{id}:body").into()))
                    .max_h(px(look.max_height))
                    .overflow_y_scroll()
                    .child(body_text(&id, text, palette, look.prose)),
            ))
        })
        .into_any_element()
}

fn body_text(id: &str, text: &str, palette: &Palette, prose: bool) -> TextView {
    let view = markdown_view::markdown(&format!("{id}:text"), text, palette, Author::Assistant);
    if prose {
        return view;
    }
    // The reasoning variant: upright sans at the meta size in the tools' color.
    view.text_size(rems(META_SIZE))
        .text_color(palette.muted_foreground)
}

/// The tool icon, replaced by the fold chevron under the pointer and while
/// the block is open.
pub(crate) fn toggle_icon(
    group: &SharedString,
    icon: &'static str,
    color: Hsla,
    expanded: bool,
    palette: &Palette,
) -> AnyElement {
    let chevron = if expanded {
        "arrow-down-s"
    } else {
        "arrow-right-s"
    };
    div()
        .relative()
        .flex_shrink_0()
        .w(px(14.0))
        .h(px(20.0))
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .when(expanded, |this| this.invisible())
                .group_hover(group.clone(), |style| style.invisible())
                .child(glyph(icon, 14.0, color)),
        )
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .when(!expanded, |this| this.invisible())
                .group_hover(group.clone(), |style| style.visible())
                .child(glyph(chevron, 14.0, palette.tools_icon)),
        )
        .into_any_element()
}

fn dots_element(dots: [bool; 3], direction: Direction) -> AnyElement {
    let mut row = div()
        .flex()
        .flex_row_from(direction)
        .margin_start(direction, px(4.0));
    for shown in dots {
        row = row.child(div().when(!shown, |this| this.invisible()).child("."));
    }
    row.into_any_element()
}

/// `BlockBody`: the expanded body of a collapsible block, beside the block
/// line that folds it. The line is a 12 px click strip around a 1 px rule.
pub(crate) fn block_body(
    id: SharedString,
    palette: &Palette,
    direction: Direction,
    cx: &gpui_kit::Context<Workspace>,
    toggle: impl Fn(&mut Workspace, &mut gpui_kit::Window, &mut gpui_kit::Context<Workspace>) + 'static,
    content: impl IntoElement,
) -> AnyElement {
    let line = div()
        .id(ElementId::Name(id))
        .absolute()
        .top_0()
        .bottom_0()
        .inset_start(direction, px(-6.0))
        .w(px(12.0))
        .cursor_pointer()
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .inset_start(direction, px(6.0))
                .w(px(1.0))
                .bg(palette.tools_border),
        );
    div()
        .relative()
        .margin_start(direction, px(8.0))
        .padding_start(direction, px(12.0))
        .pt_1()
        .pb_2()
        .child(line.on_click(cx.listener(move |this, _, window, cx| toggle(this, window, cx))))
        .child(content)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_dots_never_show_all_empty() {
        for second in -3..12 {
            assert!(busy_dots(second).iter().any(|shown| *shown));
        }
        assert_eq!(busy_dots(0), [true, false, false]);
        assert_eq!(busy_dots(4), [false, false, true]);
        assert_eq!(busy_dots(5), busy_dots(0));
    }
}
