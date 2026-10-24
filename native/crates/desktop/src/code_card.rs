//! Fenced code as the web's code card (`MarkdownCode.tsx` and the
//! `[data-streamdown="code-block"]` rules in `index.css`): a header with the
//! language and, in an assistant's message, the wrap and copy actions; line
//! numbers in a gutter; and the code in a nested Kit text view, which keeps it
//! selectable and highlighted with the theme's syntax colors. The card is a
//! custom block, so a selection running across it takes it whole.

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::base::text::{
    MarkdownNode, MarkdownParseContext, MarkdownPlugin, TextView, TextViewStyle, markdown_ast,
};
use gpui_kit::component::Theme;
use gpui_kit::{
    AccessibleAction, AnyElement, App, Bounds, ClipboardItem, ElementId, FontFeatures, Global,
    InteractiveElement, IntoElement, ParentElement, Pixels, Role, SharedString,
    StatefulInteractiveElement, StyleRefinement, Styled, TextAlign, TextRun, Toggled, Window,
    canvas, div, font, point, px, rems, transparent_black,
};

use crate::chrome::glyph;
use crate::locale;
use crate::markdown_view::Author;
use crate::theme::{Appearance, Palette};
use crate::workspace_view::ToggleCodeWrap;

/// The card: a 2 rem header in 13 px text, 12/23 px code inset
/// 10 px by 12 px, and a 2.75 rem gutter whose number column is 2 rem less
/// 0.75 rem and a pixel of padding: 19 px. (The web's gutter rule is a
/// background image that `index.css` clears, so none is drawn.)
const CARD_RADIUS: f32 = 16.0;
const HEADER_HEIGHT: f32 = 32.0;
const HEADER_TEXT: f32 = 13.0;
const HEADER_PADDING_X: f32 = 12.0;
const HEADER_PADDING_Y: f32 = 6.0;
const HEADER_ACTIONS_INSET: f32 = 84.0;
const CODE_SIZE: f32 = 12.0;
const CODE_LINE: f32 = 23.0;
const CODE_PADDING_X: f32 = 12.0;
const CODE_PADDING_Y: f32 = 10.0;
const GUTTER: f32 = 44.0;
const NUMBER_COLUMN: f32 = 19.0;
const ACTION_SIZE: f32 = 24.0;
const ACTION_GLYPH: f32 = 14.0;
const ACTION_RADIUS: f32 = 5.0;
/// How long the copy action shows its check.
const COPIED_FOR: Duration = Duration::from_secs(2);

/// Whether fenced code wraps (`codeBlockLineWrap`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CodeWrap(pub bool);

impl Default for CodeWrap {
    fn default() -> Self {
        Self(true)
    }
}

impl Global for CodeWrap {}

/// Draws fenced code as the web's code card; Mermaid keeps Kit's block.
pub(crate) struct CodeFence {
    pub(crate) author: Author,
}

/// A fenced block's language and text.
struct Fence {
    language: Option<SharedString>,
    code: SharedString,
}

const CODE_FENCE: &str = "openchamber-code";

impl MarkdownPlugin for CodeFence {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        CODE_FENCE
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown_ast::Node::Code(code) = node else {
            return None;
        };
        if code.lang.as_deref() == Some("mermaid") {
            return None;
        }
        let fence = Fence {
            language: code.lang.clone().map(SharedString::from),
            code: code.value.clone().into(),
        };
        Some(
            MarkdownNode::new(CODE_FENCE, fence)
                .text(code.value.clone())
                .markdown(cx.node_source(node).unwrap_or_default().to_owned()),
        )
    }

    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement {
        match node.data::<Fence>() {
            Some(fence) => {
                let key = node.source_range().map_or(0, |range| range.start);
                code_card(fence, key, self.author, window, cx)
            }
            None => div().into_any_element(),
        }
    }
}

fn code_card(
    fence: &Fence,
    key: usize,
    author: Author,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let palette = cx.global::<Palette>().clone();
    let mono = Theme::global(cx).mono_font_family.clone();
    let wrap = cx.try_global::<CodeWrap>().copied().unwrap_or_default().0;
    let label = fence
        .language
        .as_ref()
        .filter(|language| !language.is_empty())
        .map_or_else(|| "text".to_owned(), |language| language.to_lowercase());
    let header = div()
        .min_h(px(HEADER_HEIGHT))
        .px(px(HEADER_PADDING_X))
        .py(px(HEADER_PADDING_Y))
        .pr(px(HEADER_ACTIONS_INSET))
        .flex()
        .items_center()
        .border_b_1()
        .border_color(palette.border.opacity(0.7))
        .text_size(px(HEADER_TEXT))
        .text_color(palette.elevated_foreground)
        .child(div().font_family(mono.clone()).child(label));
    let actions =
        (author == Author::Assistant).then(|| code_actions(fence, key, wrap, &palette, window, cx));
    let code = fence.code.clone();
    let gutter_palette = palette.clone();
    let gutter_mono = mono.clone();
    let gutter = canvas(
        |_, _, _| (),
        move |bounds, (), window, cx| {
            paint_gutter(
                bounds,
                &code,
                wrap,
                &gutter_mono,
                &gutter_palette,
                window,
                cx,
            );
        },
    )
    .absolute()
    .inset_0();
    let body = div()
        .relative()
        .px(px(CODE_PADDING_X))
        .py(px(CODE_PADDING_Y))
        .child(gutter)
        .child(
            TextView::markdown(
                ElementId::Name(format!("code-{key}").into()),
                fenced(fence.language.as_deref(), &fence.code),
            )
            .style(code_style(&palette, wrap))
            .selectable(true)
            // Actions in place of Kit's language label, which the header
            // already shows.
            .code_block_actions(|_, _, _| div()),
        );
    div()
        .id(ElementId::Name(format!("code-card-{key}").into()))
        .relative()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .rounded(px(CARD_RADIUS))
        .border_1()
        .border_color(palette.border.opacity(0.8))
        .bg(palette.elevated)
        .text_color(palette.elevated_foreground)
        .flex()
        .flex_col()
        .child(header)
        .children(actions)
        .child(body)
        .into_any_element()
}

/// The wrap and copy actions at the header's end.
fn code_actions(
    fence: &Fence,
    key: usize,
    wrap: bool,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let copied = window.use_keyed_state(
        ElementId::Name(format!("code-copied-{key}").into()),
        cx,
        |_, _| false,
    );
    let is_copied = *copied.read(cx);
    let strings = locale::strings(cx);
    let wrap_title = strings.get(if wrap {
        "markdownRenderer.code.actions.disableWrapTitle"
    } else {
        "markdownRenderer.code.actions.enableWrapTitle"
    });
    let copy_title = strings.get(if is_copied {
        "markdownRenderer.code.actions.copiedTitle"
    } else {
        "markdownRenderer.code.actions.copyTitle"
    });
    let toggle_wrap = |_: &_, window: &mut Window, cx: &mut App| {
        window.dispatch_action(Box::new(ToggleCodeWrap), cx);
    };
    let code = fence.code.clone();
    let copy = move |_: &_, window: &mut Window, cx: &mut App| {
        cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
        copied.update(cx, |copied, _| *copied = true);
        window.refresh();
        let copied = copied.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            cx.update(|cx| {
                copied.update(cx, |copied, _| *copied = false);
                cx.refresh_windows();
            });
        })
        .detach();
    };
    let wrap_button = action_button(
        ElementId::Name(format!("code-wrap-{key}").into()),
        "text-wrap",
        wrap_title,
        palette,
    )
    .aria_toggled(if wrap { Toggled::True } else { Toggled::False })
    .on_click(move |event, window, cx| toggle_wrap(event, window, cx))
    .on_a11y_action(AccessibleAction::Click, move |_, window, cx| {
        window.dispatch_action(Box::new(ToggleCodeWrap), cx);
    });
    let copy_button = action_button(
        ElementId::Name(format!("code-copy-{key}").into()),
        if is_copied { "check" } else { "file-copy" },
        copy_title,
        palette,
    );
    let copy_click = Rc::new(copy);
    let pointer_copy = copy_click.clone();
    let copy_button = copy_button
        .on_click(move |_, window, cx| pointer_copy(&(), window, cx))
        .on_a11y_action(AccessibleAction::Click, move |_, window, cx| {
            copy_click(&(), window, cx);
        });
    div()
        .absolute()
        .top_0()
        .right_0()
        .h(px(HEADER_HEIGHT))
        .flex()
        .items_center()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(2.0))
                .px(px(6.0))
                .py(px(4.0))
                .child(wrap_button)
                .child(copy_button),
        )
        .into_any_element()
}

fn action_button(
    id: ElementId,
    icon: &str,
    title: SharedString,
    palette: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let hover = palette.hover;
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(title)
        .size(px(ACTION_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(ACTION_RADIUS))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .child(glyph(icon, ACTION_GLYPH, palette.elevated_foreground))
}

/// The code as a fence of its own, with a fence longer than any backtick
/// run inside it.
fn fenced(language: Option<&str>, code: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for character in code.chars() {
        if character == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{}\n{code}\n{fence}", language.unwrap_or_default())
}

/// The nested view's code block: no chrome of its own, the gutter's inset,
/// 12/23 px text, and wrapping as the setting says.
fn code_style(palette: &Palette, wrap: bool) -> TextViewStyle {
    let mut block = StyleRefinement::default()
        .p_0()
        .pl(px(GUTTER))
        .bg(transparent_black())
        .border_0()
        .rounded_none()
        .text_size(px(CODE_SIZE))
        .line_height(px(CODE_LINE));
    if !wrap {
        block = block.whitespace_nowrap();
    }
    TextViewStyle::default()
        .with_foreground(palette.syntax_foreground)
        .with_muted_foreground(palette.muted_foreground)
        .with_selection(palette.selection)
        .with_border(palette.border)
        .with_paragraph_gap(rems(0.0))
        .with_code_block(block)
        .with_dark(palette.appearance == Appearance::Dark)
}

/// Paints the line numbers beside the code. A wrapped line takes as many
/// rows as its text wraps to at the code's width, so each number stays
/// beside the first row of its line. Numbers align to the column's end; one
/// wider than the column starts at its start and runs past it, as CSS lays
/// out right-aligned text that overflows.
fn paint_gutter(
    bounds: Bounds<Pixels>,
    code: &str,
    wrap: bool,
    mono: &SharedString,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) {
    let code_width = bounds.size.width - px(2.0 * CODE_PADDING_X + GUTTER);
    let line_height = px(CODE_LINE);
    let font_size = px(CODE_SIZE);
    let text_font = font(mono.clone());
    let mut number_font = text_font.clone();
    number_font.features = FontFeatures(std::sync::Arc::new(vec![("tnum".to_owned(), 1)]));
    let number_color = palette.muted_foreground.opacity(0.45);
    let number_start = bounds.left() + px(CODE_PADDING_X);
    let number_end = number_start + px(NUMBER_COLUMN);
    let mut y = bounds.top() + px(CODE_PADDING_Y);
    for (index, line) in code.split('\n').enumerate() {
        let rows = if wrap && !line.is_empty() {
            let run = TextRun {
                len: line.len(),
                font: text_font.clone(),
                color: palette.syntax_foreground,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .shape_text(
                    SharedString::from(line.to_owned()),
                    font_size,
                    &[run],
                    Some(code_width),
                    None,
                )
                .map_or(1, |lines| {
                    lines
                        .iter()
                        .map(|line| line.wrap_boundaries().len() + 1)
                        .sum()
                })
        } else {
            1
        };
        let number = (index + 1).to_string();
        let run = TextRun {
            len: number.len(),
            font: number_font.clone(),
            color: number_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window
            .text_system()
            .shape_line(number.into(), font_size, &[run], None);
        let origin = point((number_end - shaped.width).max(number_start), y);
        let _ = shaped.paint(origin, line_height, TextAlign::Left, None, window, cx);
        y += line_height * rows as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fence_outruns_the_backticks_inside_it() {
        assert_eq!(fenced(Some("ts"), "let a = 1;"), "```ts\nlet a = 1;\n```");
        assert_eq!(fenced(None, "a ```` b"), "`````\na ```` b\n`````");
    }
}
