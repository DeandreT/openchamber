//! Message Markdown as the web's `MarkdownRenderer` draws it.
//!
//! Text goes through Kit's text view in the chat's body color with the web's
//! 0.65 em between blocks. Headings, lists and fenced code are custom blocks
//! drawn to the `.markdown-content` rules in `index.css` and
//! `design-system.css`; their inline text goes through nested text views built
//! here, so links, emphasis and code inside them render as in a paragraph. A
//! selection running across a custom block takes it whole; one inside it
//! stays within its nested view.

use gpui_kit::base::text::{
    MarkdownNode, MarkdownParseContext, MarkdownPlugin, TextView, TextViewStyle, markdown_ast,
};
use gpui_kit::{
    App, ElementId, FontWeight, HighlightStyle, Hsla, IntoElement, ParentElement, Styled, Window,
    div, relative, rems,
};

use crate::code_card::CodeFence;
use crate::markdown_list::{ListDepth, MarkdownList};
use crate::theme::{Appearance, Palette};

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
    TextView::markdown(id, text)
        .style(body_style(palette, body_color(palette, nesting.author)))
        .selectable(true)
        .text_size(rems(BODY_SIZE))
        .plugin(CodeFence {
            author: nesting.author,
        })
        .plugin(MarkdownHeading { nesting })
        .plugin(MarkdownList { nesting })
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
            background_color: Some(palette.hover),
            ..HighlightStyle::default()
        })
        .with_dark(palette.appearance == Appearance::Dark)
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
        let text = view(
            ElementId::Name(format!("heading-{key}").into()),
            heading.text.clone(),
            &palette,
            self.nesting,
        )
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
    fn heading_text_never_starts_a_block() {
        assert_eq!(inline_only("3. Why it matters"), "3\\. Why it matters");
        assert_eq!(inline_only("- not a list"), "\\- not a list");
        assert_eq!(inline_only("Plain *text*"), "Plain *text*");
        assert_eq!(inline_only("2024 roadmap"), "2024 roadmap");
    }
}
