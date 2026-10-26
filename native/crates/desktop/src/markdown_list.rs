//! Lists as the web draws them (`.markdown-content` list rules in
//! `design-system.css`): every line of an item starts at the item's inset,
//! 1.25 rem for bullets and 2 em for numbers, with the marker outside it in
//! the text color; items stand 0.25 rem apart. Nesting cycles the marker:
//! disc, circle, square for bullets and decimal, letter, roman numeral for
//! numbers. Each item's content is a nested text view, so paragraphs, code
//! and further lists inside it take the same rules.
//!
//! Task lists, and lists inside a block quote (whose item text carries the
//! quote's markers), stay with Kit's own list.

use gpui_kit::base::text::{MarkdownNode, MarkdownParseContext, MarkdownPlugin, markdown_ast};
use gpui_kit::{
    App, ElementId, FontFeatures, IntoElement, ParentElement, Styled, WeakEntity, Window, div,
    prelude::FluentBuilder, px, rems,
};

use crate::markdown_view::{self, Nesting};
use crate::theme::Palette;
use crate::transcript_selection::{self, RetainedText};

/// Item insets: 1.25 rem for bullets, 2 em of the 14 px body for numbers.
const BULLET_INSET: f32 = 1.25;
const NUMBER_INSET: f32 = 1.75;
const ITEM_GAP: f32 = 0.25;
/// Chromium's outside disc on the 23 px body line: 5 px across, its start
/// 17 px before the text, centered on the line.
const DISC_SIZE: f32 = 5.0;
const DISC_OFFSET: f32 = 17.0;
const BODY_LINE: f32 = 23.0;

/// How many bulleted and numbered lists enclose a view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ListDepth {
    pub(crate) bulleted: u8,
    pub(crate) numbered: u8,
}

impl ListDepth {
    fn inside(self, kind: ListKind) -> Self {
        match kind {
            ListKind::Bulleted => Self {
                bulleted: self.bulleted.saturating_add(1),
                ..self
            },
            ListKind::Numbered { .. } => Self {
                numbered: self.numbered.saturating_add(1),
                ..self
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListKind {
    Bulleted,
    Numbered { start: u32 },
}

/// A list's kind and each item's Markdown, dedented to the item's content.
struct List {
    kind: ListKind,
    items: Vec<String>,
}

/// Draws lists as the web does.
pub(crate) struct MarkdownList {
    pub(crate) nesting: Nesting,
    pub(crate) owner: Option<WeakEntity<RetainedText>>,
}

const LIST: &str = "openchamber-list";

impl MarkdownPlugin for MarkdownList {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        LIST
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown_ast::Node::List(list) = node else {
            return None;
        };
        let mut items = Vec::with_capacity(list.children.len());
        for child in &list.children {
            let markdown_ast::Node::ListItem(item) = child else {
                return None;
            };
            if item.checked.is_some() {
                return None;
            }
            items.push(item_source(item, cx)?);
        }
        let kind = if list.ordered {
            ListKind::Numbered {
                start: list.start.unwrap_or(1),
            }
        } else {
            ListKind::Bulleted
        };
        let text = items.join("\n");
        Some(
            MarkdownNode::new(LIST, List { kind, items })
                .text(text)
                .markdown(cx.node_source(node).unwrap_or_default().to_owned()),
        )
    }

    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Some(list) = node.data::<List>() else {
            return div().into_any_element();
        };
        let palette = cx.global::<Palette>().clone();
        let color = markdown_view::body_color(&palette, self.nesting.author);
        let key = node.source_range().map_or(0, |range| range.start);
        let inner = Nesting {
            lists: self.nesting.lists.inside(list.kind),
            ..self.nesting
        };
        let inset = match list.kind {
            ListKind::Bulleted => BULLET_INSET,
            ListKind::Numbered { .. } => NUMBER_INSET,
        };
        let space = space_width(window);
        let rows = list.items.iter().enumerate().map(|(index, item)| {
            let retained = self.owner.as_ref().and_then(|owner| {
                node.source_range().and_then(|range| {
                    transcript_selection::child(owner, range, index, item, Some(inner), cx)
                })
            });
            let text = retained
                .map(|view| markdown_view::styled(view, &palette, inner.author))
                .unwrap_or_else(|| {
                    markdown_view::view(
                        ElementId::Name(format!("list-{key}-{index}").into()),
                        item.clone(),
                        &palette,
                        inner,
                    )
                });
            let marker = match list.kind {
                ListKind::Bulleted => bullet(self.nesting.lists.bulleted, inset, color, window),
                ListKind::Numbered { start } => {
                    let number = start.saturating_add(index as u32);
                    let label = format!("{}.", numeral(number, self.nesting.lists.numbered));
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .w(rems(inset))
                        .pr(space)
                        .flex()
                        .justify_end()
                        .text_color(color)
                        .font_features(FontFeatures(std::sync::Arc::new(vec![(
                            "tnum".to_owned(),
                            1,
                        )])))
                        .child(label)
                        .into_any_element()
                }
            };
            div()
                .relative()
                .pl(rems(inset))
                .when(index > 0, |this| this.mt(rems(ITEM_GAP)))
                .child(marker)
                .child(text)
        });
        div().flex().flex_col().children(rows).into_any_element()
    }
}

/// The bullet for a depth: disc, then circle, then square.
fn bullet(depth: u8, inset: f32, color: gpui_kit::Hsla, window: &Window) -> gpui_kit::AnyElement {
    let inset = rems(inset).to_pixels(window.rem_size());
    let shape = div()
        .absolute()
        .left(inset - px(DISC_OFFSET))
        .top(px((BODY_LINE - DISC_SIZE) / 2.0))
        .size(px(DISC_SIZE));
    match depth {
        0 => shape.rounded_full().bg(color),
        1 => shape.rounded_full().border_1().border_color(color),
        _ => shape.bg(color),
    }
    .into_any_element()
}

/// A number as the depth writes it: decimal, then lower letters, then lower
/// roman numerals.
fn numeral(number: u32, depth: u8) -> String {
    match depth {
        0 => number.to_string(),
        1 => alphabetic(number),
        _ => roman(number),
    }
}

/// `lower-alpha`: a to z, then aa, ab and so on; 0 stays a number.
fn alphabetic(number: u32) -> String {
    if number == 0 {
        return number.to_string();
    }
    let mut remaining = number;
    let mut letters = Vec::new();
    while remaining > 0 {
        remaining -= 1;
        letters.push(char::from(b'a' + (remaining % 26) as u8));
        remaining /= 26;
    }
    letters.iter().rev().collect()
}

/// `lower-roman` for 1 to 3999; other numbers stay decimal, as in CSS.
fn roman(number: u32) -> String {
    if !(1..=3999).contains(&number) {
        return number.to_string();
    }
    const NUMERALS: [(u32, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut remaining = number;
    let mut text = String::new();
    for (value, numeral) in NUMERALS {
        while remaining >= value {
            text.push_str(numeral);
            remaining -= value;
        }
    }
    text
}

/// The width of a space in the body font: the gap between a number marker
/// and the item, as in the `1. ` marker CSS draws.
fn space_width(window: &mut Window) -> gpui_kit::Pixels {
    let style = window.text_style();
    let size = rems(markdown_view::BODY_SIZE).to_pixels(window.rem_size());
    let run = style.to_run(1);
    window
        .text_system()
        .layout_line(" ", size, &[run], None)
        .width
}

/// An item's Markdown with its marker removed and its continuation lines
/// dedented to the content's column; `None` for an item inside a block quote
/// or indented with tabs, which Kit's list keeps.
fn item_source(item: &markdown_ast::ListItem, cx: &MarkdownParseContext<'_>) -> Option<String> {
    let Some(first) = item.children.first().and_then(markdown_ast::Node::position) else {
        return Some(String::new());
    };
    let source = cx.source();
    let line_start = source
        .get(..first.start.offset)?
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    if source.get(line_start..first.start.offset)?.contains('>') {
        return None;
    }
    let text = markdown_view::children_source(&item.children, cx);
    let indent = first.start.column.saturating_sub(1);
    let mut lines = text.split('\n');
    let mut dedented = lines.next().unwrap_or_default().to_owned();
    for line in lines {
        if line.starts_with('\t') {
            return None;
        }
        let spaces = line.len() - line.trim_start_matches(' ').len();
        dedented.push('\n');
        dedented.push_str(&line[spaces.min(indent)..]);
    }
    Some(dedented)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_numbers_cycle_letters_and_numerals() {
        assert_eq!(numeral(3, 0), "3");
        assert_eq!(numeral(1, 1), "a");
        assert_eq!(numeral(27, 1), "aa");
        assert_eq!(numeral(14, 2), "xiv");
        assert_eq!(numeral(4000, 2), "4000");
    }
}
