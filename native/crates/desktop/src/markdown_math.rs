//! Message math (`@streamdown/math` with `remarkMathCompatibility.ts`).
//!
//! `$…$` is inline math and `$$…$$` display math, laid out in display style
//! also inside a paragraph, where it stays in the line. A single-dollar span that
//! reads as money (`$5 and $10`) stays prose, as the web's tokenizer rejects
//! it. Formulas lay out with RaTeX, KaTeX's layout rules in Rust, and draw as
//! an SVG image with KaTeX's own glyph outlines in the body text color. A
//! formula RaTeX cannot parse shows its source.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::base::text::{
    InlineElement, InlineRenderContext, MarkdownNode, MarkdownParseContext, MarkdownPlugin,
    markdown_ast,
};
use gpui_kit::component::Theme;
use gpui_kit::{
    AnyElement, App, Hsla, Image, ImageFormat, IntoElement, ParentElement, Rgba, Styled, Window,
    div, img, px,
};
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_svg::{SvgOptions, render_to_svg};
use ratex_types::{Color, MathStyle};

use crate::markdown_view::Author;
use crate::theme::Palette;

const INLINE_MATH: &str = "inline-math";
const DISPLAY_MATH: &str = "display-math";
/// Body text is 14 px; KaTeX draws math at 1.21 em of it.
const MATH_FONT_SIZE: f64 = 14.0 * 1.21;
/// Rendered formulas kept per process: the web's KaTeX keeps none, but
/// laying a formula out again on every frame would cost a parse each time.
const CACHE_LIMIT: usize = 256;

/// One formula as the source wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Formula {
    source: String,
    display: bool,
}

/// A laid-out formula: its image and size in pixels.
#[derive(Clone)]
struct Rendered {
    image: Arc<Image>,
    width: f32,
    height: f32,
    /// How far the formula reaches below the text baseline.
    depth: f32,
}

type CacheKey = (String, bool, u32);

thread_local! {
    // Formulas render while the window draws, on the main thread.
    static CACHE: RefCell<HashMap<CacheKey, Option<Rendered>>> = RefCell::new(HashMap::new());
}

/// `$…$` and `$$…$$`, inline and in their own block.
pub(crate) struct MarkdownMath {
    pub(crate) author: Author,
}

impl MarkdownPlugin for MarkdownMath {
    fn name(&self) -> &str {
        INLINE_MATH
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown_ast::Node::InlineMath(math) = node else {
            return None;
        };
        let source = cx.node_source(node).unwrap_or_default();
        // `remarkMathCompatibility.ts` renders a `$$…$$` span in display
        // mode; a single-dollar span that reads as money stays prose.
        let display = source.starts_with("$$");
        if !display && reads_as_money(&math.value, preceding(cx, node)) {
            return None;
        }
        Some(
            MarkdownNode::new(
                INLINE_MATH,
                Formula {
                    source: math.value.clone(),
                    display,
                },
            )
            .text(math.value.clone())
            .markdown(source.to_owned()),
        )
    }

    fn render(&self, node: &MarkdownNode, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        formula_element(node, self.author, cx)
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        _context: &InlineRenderContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let formula = node.data::<Formula>()?;
        let palette = cx.global::<Palette>().clone();
        let color = crate::markdown_view::body_color(&palette, self.author);
        let element = match rendered(formula, color) {
            Some(rendered) => {
                let baseline = rendered.height - rendered.depth;
                return Some(InlineElement::new(math_image(&rendered)).with_baseline(px(baseline)));
            }
            None => source_element(formula, &palette, cx),
        };
        Some(InlineElement::new(element))
    }
}

/// `$$…$$` on lines of its own.
pub(crate) struct MarkdownDisplayMath {
    pub(crate) author: Author,
}

impl MarkdownPlugin for MarkdownDisplayMath {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        DISPLAY_MATH
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown_ast::Node::Math(math) = node else {
            return None;
        };
        Some(
            MarkdownNode::new(
                DISPLAY_MATH,
                Formula {
                    source: math.value.clone(),
                    display: true,
                },
            )
            .text(math.value.clone())
            .markdown(cx.node_source(node).unwrap_or_default().to_owned()),
        )
    }

    fn render(&self, node: &MarkdownNode, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        // `.katex-display`: centered, scrolling sideways when wider than the
        // column, and left to right in every locale.
        div()
            .w_full()
            .flex()
            .justify_center()
            .overflow_x_hidden()
            .child(formula_element(node, self.author, cx))
    }
}

fn formula_element(node: &MarkdownNode, author: Author, cx: &mut App) -> AnyElement {
    let Some(formula) = node.data::<Formula>() else {
        return div().into_any_element();
    };
    let palette = cx.global::<Palette>().clone();
    let color = crate::markdown_view::body_color(&palette, author);
    match rendered(formula, color) {
        Some(rendered) => math_image(&rendered),
        None => source_element(formula, &palette, cx),
    }
}

fn math_image(rendered: &Rendered) -> AnyElement {
    img(rendered.image.clone())
        .w(px(rendered.width))
        .h(px(rendered.height))
        .flex_shrink_0()
        .into_any_element()
}

/// KaTeX's `throwOnError: false`: the source in the error color.
fn source_element(formula: &Formula, palette: &Palette, cx: &App) -> AnyElement {
    let mono = Theme::global(cx).mono_font_family.clone();
    div()
        .font_family(mono)
        .text_color(palette.status_error)
        .child(if formula.display {
            format!("$${}$$", formula.source)
        } else {
            format!("${}$", formula.source)
        })
        .into_any_element()
}

/// The formula drawn in `color`, from the cache when it was drawn before.
fn rendered(formula: &Formula, color: Hsla) -> Option<Rendered> {
    let rgba = Rgba::from(color);
    let key = (formula.source.clone(), formula.display, u32::from(rgba));
    if let Some(entry) = CACHE.with_borrow(|cache| cache.get(&key).cloned()) {
        return entry;
    }
    let entry = render_formula(&formula.source, formula.display, rgba);
    CACHE.with_borrow_mut(|cache| {
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, entry.clone());
    });
    entry
}

fn render_formula(source: &str, display: bool, color: Rgba) -> Option<Rendered> {
    let nodes = ratex_parser::parse(source).ok()?;
    let options = LayoutOptions {
        style: if display {
            MathStyle::Display
        } else {
            MathStyle::Text
        },
        color: Color {
            r: color.r,
            g: color.g,
            b: color.b,
            a: color.a,
        },
        ..LayoutOptions::default()
    };
    let list = to_display_list(&layout(&nodes, &options));
    if list.width <= 0.0 {
        return None;
    }
    let svg = render_to_svg(
        &list,
        &SvgOptions {
            font_size: MATH_FONT_SIZE,
            padding: 0.0,
            embed_glyphs: true,
            ..SvgOptions::default()
        },
    );
    Some(Rendered {
        image: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())),
        width: (list.width * MATH_FONT_SIZE) as f32,
        height: ((list.height + list.depth) * MATH_FONT_SIZE) as f32,
        depth: (list.depth * MATH_FONT_SIZE) as f32,
    })
}

/// The character before a node in the message source.
fn preceding(cx: &MarkdownParseContext<'_>, node: &markdown_ast::Node) -> Option<char> {
    let start = node.position()?.start.offset;
    cx.source().get(..start)?.chars().next_back()
}

/// Whether a single-dollar span is money and not math (`tokenizeDollarMath`):
/// it follows a digit, starts or ends with space, holds only figures, or
/// starts with a figure followed by `;`, `:`, ` and` or ` to`.
fn reads_as_money(value: &str, before: Option<char>) -> bool {
    if before.is_some_and(|character| character.is_ascii_digit()) {
        return true;
    }
    if value.is_empty()
        || value.starts_with(char::is_whitespace)
        || value.ends_with(char::is_whitespace)
    {
        return true;
    }
    if value
        .chars()
        .all(|character| character.is_ascii_digit() || matches!(character, '.' | ',' | ' '))
    {
        return true;
    }
    let figures = value
        .find(|character: char| !(character.is_ascii_digit() || matches!(character, '.' | ',')))
        .unwrap_or(value.len());
    if figures == 0 || !value.starts_with(|character: char| character.is_ascii_digit()) {
        return false;
    }
    let rest = &value[figures..];
    rest.starts_with([';', ':'])
        || ["and", "to"].iter().any(|word| {
            let trimmed = rest.trim_start();
            trimmed.len() < rest.len()
                && trimmed.strip_prefix(word).is_some_and(|after| {
                    after.is_empty() || !after.starts_with(|c: char| c.is_alphanumeric())
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_stays_prose() {
        assert!(reads_as_money("5 and ", None));
        assert!(reads_as_money("10", None));
        assert!(reads_as_money(" x", None));
        assert!(reads_as_money("x", Some('3')));
        assert!(reads_as_money("5 to 10", None));
        assert!(reads_as_money("3;", None));
        assert!(!reads_as_money("E = mc^2", None));
        assert!(!reads_as_money("2x", None));
        assert!(!reads_as_money("5 + andrew", None));
    }

    #[test]
    fn formulas_render_once_and_errors_render_none() {
        let color = Rgba {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        };
        let fraction = render_formula(r"\frac{1}{3}", true, color).expect("a fraction lays out");
        assert!(fraction.width > 0.0 && fraction.height > fraction.depth);
        let inline = render_formula("a^2", false, color).expect("inline math lays out");
        assert!(
            inline.height < fraction.height,
            "text style is smaller than display"
        );
        assert!(render_formula(r"\frac{1}{", true, color).is_none());
        let formula = Formula {
            source: "x_1".to_owned(),
            display: false,
        };
        let first = rendered(&formula, color.into()).expect("renders");
        let again = rendered(&formula, color.into()).expect("cached");
        assert!(
            Arc::ptr_eq(&first.image, &again.image),
            "the cache returns the same image"
        );
    }
}
