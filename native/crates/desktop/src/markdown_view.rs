//! Message Markdown as the web's `MarkdownRenderer` draws it.
//!
//! Text goes through Kit's text view in the chat's body color with the web's
//! 0.65 em between blocks. Headings, lists and fenced code are custom blocks
//! drawn to the `.markdown-content` rules in `index.css` and
//! `design-system.css`; their inline text goes through nested text views built
//! here, so links, emphasis and code inside them render as in a paragraph. A
//! selection running across a custom block takes it whole; one inside it
//! stays within its nested view.

use std::sync::Arc;

use gpui_kit::base::text::{
    MarkdownExtensions, MarkdownNode, MarkdownParseContext, MarkdownPlugin, TextView,
    TextViewStyle, markdown_ast,
};
use gpui_kit::{
    App, ElementId, FontWeight, HighlightStyle, Hsla, IntoElement, ParentElement, StyleRefinement,
    Styled, WeakEntity, Window, div, relative, rems, transparent_black,
};

use crate::code_card::CodeFence;
use crate::markdown_diagram::MarkdownDiagram;
use crate::markdown_list::{ListDepth, MarkdownList};
use crate::markdown_math::{MarkdownDisplayMath, MarkdownMath};
use crate::theme::{Appearance, Palette};
use crate::transcript_selection::{self, RetainedText};

/// Body text: 14 px at 90 % of the foreground, 0.65 em between blocks.
pub(crate) const BODY_SIZE: f32 = 0.875;
const BODY_ALPHA: f32 = 0.9;
const BLOCK_GAP: f32 = 0.65;

/// Headings: semibold at 95 % of the foreground on a 1.3 line, with
/// 1.25 rem above them except at the start; the block gap below a heading
/// stands for its 0.5 rem margin, which collapses into the next block's.
const HEADING_ALPHA: f32 = 0.95;
const HEADING_LINE: f32 = 1.3;
const HEADING_SPACE_ABOVE: f32 = 1.25;

/// Tables, as `[data-streamdown="table"]` and its cells in `index.css`: the
/// frame has `0.5rem` corners, the elevated surface and a border at 80 %;
/// cells pad `0.625rem 1rem` and divide columns at 60 % of the border.
const TABLE_RADIUS: f32 = 0.5;
const TABLE_BORDER_ALPHA: f32 = 0.8;
const TABLE_CELL_PADDING_X: f32 = 1.0;
const TABLE_CELL_PADDING_Y: f32 = 0.625;
const TABLE_DIVIDER_ALPHA: f32 = 0.6;

/// Whose message the Markdown belongs to; a user's code has no actions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Author {
    #[default]
    Assistant,
    User,
}

/// Where a nested view sits: the message's author and the lists around it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Nesting {
    pub(crate) author: Author,
    pub(crate) lists: ListDepth,
}

/// A message's Markdown.
pub(crate) fn markdown(id: &str, text: &str, palette: &Palette, author: Author) -> TextView {
    view(
        ElementId::Name(id.to_owned().into()),
        text.to_owned(),
        palette,
        Nesting {
            author,
            lists: ListDepth::default(),
        },
    )
}

/// A text view of `text` with the message's styles and custom blocks; also
/// the view inside a heading or list item.
pub(crate) fn view(id: ElementId, text: String, palette: &Palette, nesting: Nesting) -> TextView {
    styled(
        TextView::markdown(id, text)
            .markdown_extensions(extensions(nesting, None).as_ref().clone()),
        palette,
        nesting.author,
    )
}

pub(crate) fn styled(view: TextView, palette: &Palette, author: Author) -> TextView {
    view.on_link_click(crate::markdown_links::on_markdown_link_click)
        .style(body_style(palette, body_color(palette, author)))
        .selectable(true)
        .text_size(rems(BODY_SIZE))
}

pub(crate) fn extensions(
    nesting: Nesting,
    owner: Option<WeakEntity<RetainedText>>,
) -> Arc<MarkdownExtensions> {
    Arc::new(
        MarkdownExtensions::default()
            .plugin(CodeFence {
                author: nesting.author,
                owner: owner.clone(),
            })
            .plugin(MarkdownHeading {
                nesting,
                owner: owner.clone(),
            })
            .plugin(MarkdownList { nesting, owner })
            .plugin(MarkdownDiagram)
            .plugin(MarkdownDisplayMath {
                author: nesting.author,
            })
            .plugin(MarkdownMath {
                author: nesting.author,
            }),
    )
}

/// The body text color of a message by `author`.
pub(crate) fn body_color(palette: &Palette, author: Author) -> Hsla {
    match author {
        Author::Assistant => palette.foreground.opacity(BODY_ALPHA),
        Author::User => palette.foreground,
    }
}

fn body_style(palette: &Palette, foreground: Hsla) -> TextViewStyle {
    TextViewStyle::default()
        .with_foreground(foreground)
        .with_muted_foreground(palette.muted_foreground)
        .with_link(palette.link)
        .with_selection(palette.selection)
        .with_code_background(palette.muted)
        .with_border(palette.border)
        .with_paragraph_gap(rems(BLOCK_GAP))
        .with_inline_code(HighlightStyle {
            color: Some(palette.inline_code.opacity(0.9)),
            background_color: Some(palette.inline_code_background.opacity(0.5)),
            ..HighlightStyle::default()
        })
        .with_table(table_frame(palette))
        .with_table_head(table_head(palette))
        .with_table_cell(table_cell(palette))
        .with_dark(palette.appearance == Appearance::Dark)
}

/// The table's frame. Its text is `--text-sm`, the body's size, which also
/// sizes the columns.
fn table_frame(palette: &Palette) -> StyleRefinement {
    StyleRefinement::default()
        .rounded(rems(TABLE_RADIUS))
        .border_color(palette.border.opacity(TABLE_BORDER_ALPHA))
        .bg(palette.elevated)
        .text_size(rems(BODY_SIZE))
}

/// The header row: semibold in the full foreground, with no fill of its own.
fn table_head(palette: &Palette) -> StyleRefinement {
    StyleRefinement::default()
        .bg(transparent_black())
        .text_color(palette.foreground)
        .font_weight(FontWeight::SEMIBOLD)
}

fn table_cell(palette: &Palette) -> StyleRefinement {
    StyleRefinement::default()
        .px(rems(TABLE_CELL_PADDING_X))
        .py(rems(TABLE_CELL_PADDING_Y))
        .border_color(palette.border.opacity(TABLE_DIVIDER_ALPHA))
}

/// The source of a node's children: the inline text of a heading, the
/// content of a list item's first line.
pub(crate) fn children_source<'a>(
    children: &[markdown_ast::Node],
    cx: &MarkdownParseContext<'a>,
) -> &'a str {
    let start = children
        .first()
        .and_then(markdown_ast::Node::position)
        .map(|position| position.start.offset);
    let end = children
        .last()
        .and_then(markdown_ast::Node::position)
        .map(|position| position.end.offset);
    match (start, end) {
        (Some(start), Some(end)) => cx.source().get(start..end).unwrap_or_default(),
        _ => "",
    }
}

/// `text` with every line's block syntax escaped, so a view of it holds
/// one paragraph: a heading's `3. Why` is text, not a numbered list.
fn inline_only(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() + 2);
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            escaped.push('\n');
        }
        let content = line.trim_start_matches(' ');
        escaped.push_str(&line[..line.len() - content.len()]);
        let digits = content.len()
            - content
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .len();
        if digits > 0 && content[digits..].starts_with(['.', ')']) {
            escaped.push_str(&content[..digits]);
            escaped.push('\\');
            escaped.push_str(&content[digits..]);
        } else if content.starts_with(['-', '+', '*', '#', '>', '=', '`', '~', '|', '<', '_']) {
            escaped.push('\\');
            escaped.push_str(content);
        } else {
            escaped.push_str(content);
        }
    }
    escaped
}

/// Draws a heading at the web's size and spacing.
struct MarkdownHeading {
    nesting: Nesting,
    owner: Option<WeakEntity<RetainedText>>,
}

/// A heading's level, inline Markdown, and whether it opens the document.
struct Heading {
    level: u8,
    text: String,
    first: bool,
}

const HEADING: &str = "openchamber-heading";

impl MarkdownPlugin for MarkdownHeading {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        HEADING
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown_ast::Node::Heading(heading) = node else {
            return None;
        };
        let start = heading.position.as_ref()?.start.offset;
        let first = cx.offset() == 0 && cx.source().get(..start)?.trim().is_empty();
        let text = inline_only(children_source(&heading.children, cx));
        Some(
            MarkdownNode::new(
                HEADING,
                Heading {
                    level: heading.depth,
                    text: text.clone(),
                    first,
                },
            )
            .text(text)
            .markdown(cx.node_source(node).unwrap_or_default().to_owned()),
        )
    }

    fn render(&self, node: &MarkdownNode, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let Some(heading) = node.data::<Heading>() else {
            return div().into_any_element();
        };
        let palette = cx.global::<Palette>().clone();
        let key = node.source_range().map_or(0, |range| range.start);
        let size = match heading.level {
            1 => 1.25,
            2 => 1.125,
            3 => 1.0,
            _ => 0.875,
        };
        let color = palette.foreground.opacity(HEADING_ALPHA);
        let retained = self.owner.as_ref().and_then(|owner| {
            node.source_range().and_then(|range| {
                transcript_selection::child(owner, range, 0, &heading.text, Some(self.nesting), cx)
            })
        });
        let text = retained
            .unwrap_or_else(|| {
                view(
                    ElementId::Name(format!("heading-{key}").into()),
                    heading.text.clone(),
                    &palette,
                    self.nesting,
                )
            })
            .style(body_style(&palette, color))
            .text_size(rems(size))
            .line_height(relative(HEADING_LINE))
            .font_weight(FontWeight::SEMIBOLD);
        // The block gap above already stands for part of the space.
        let above = if heading.first {
            0.0
        } else {
            HEADING_SPACE_ABOVE - BLOCK_GAP
        };
        div().pt(rems(above)).child(text).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_code_uses_the_markdown_theme_tokens() {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let palette = Palette::default_for(appearance);
            let style = body_style(&palette, palette.foreground).inline_code();
            assert_eq!(style.color, Some(palette.inline_code.opacity(0.9)));
            assert_eq!(
                style.background_color,
                Some(palette.inline_code_background.opacity(0.5))
            );
        }
    }

    #[test]
    fn tables_use_the_reference_frame_header_and_cell_styles() {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let palette = Palette::default_for(appearance);
            let style = body_style(&palette, palette.foreground);
            let frame = style.table();
            assert_eq!(frame.text.font_size, Some(rems(0.875).into()));
            assert_eq!(frame.border_color, Some(palette.border.opacity(0.8)));
            assert_eq!(frame.background, Some(palette.elevated.into()));
            assert_eq!(frame.corner_radii.top_left, Some(rems(0.5).into()));
            assert_eq!(frame.corner_radii.bottom_right, Some(rems(0.5).into()));
            let head = style.table_head();
            assert_eq!(head.text.font_weight, Some(FontWeight::SEMIBOLD));
            assert_eq!(head.text.color, Some(palette.foreground));
            assert_eq!(head.background, Some(transparent_black().into()));
            let cell = style.table_cell();
            assert_eq!(cell.padding.left, Some(rems(1.0).into()));
            assert_eq!(cell.padding.right, Some(rems(1.0).into()));
            assert_eq!(cell.padding.top, Some(rems(0.625).into()));
            assert_eq!(cell.padding.bottom, Some(rems(0.625).into()));
            assert_eq!(cell.border_color, Some(palette.border.opacity(0.6)));
        }
    }

    /// The reference's one-line row: the 23 px body line, `0.625rem` above
    /// and below, and the 1 px row border.
    #[test]
    fn a_one_line_table_row_is_44_px_high() {
        assert_eq!(23.0 + 2.0 * TABLE_CELL_PADDING_Y * 16.0 + 1.0, 44.0);
    }

    #[test]
    fn heading_text_never_starts_a_block() {
        assert_eq!(inline_only("3. Why it matters"), "3\\. Why it matters");
        assert_eq!(inline_only("- not a list"), "\\- not a list");
        assert_eq!(inline_only("Plain *text*"), "Plain *text*");
        assert_eq!(inline_only("2024 roadmap"), "2024 roadmap");
    }
}
