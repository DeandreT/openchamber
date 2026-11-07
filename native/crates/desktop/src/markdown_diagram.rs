//! Mermaid fences (`MarkdownMermaid.tsx`).
//!
//! A completed ` ```mermaid ` fence draws as its diagram in the theme's
//! colors, with a Copy action that copies the source. A diagram lays out off
//! the main thread the first time its source and theme are seen; until then,
//! and when the source does not parse, the fence shows its source.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::base::text::{MarkdownNode, MarkdownParseContext, MarkdownPlugin, markdown_ast};
use gpui_kit::component::Theme;
use gpui_kit::{
    App, ClipboardItem, Hsla, Image, ImageFormat, IntoElement, ParentElement, Rgba, SharedString,
    Styled, TestSupportExt, Window, div, img, px, rems,
};

use crate::code_card::action_button;
use crate::locale;
use crate::theme::Palette;

const DIAGRAM: &str = "mermaid-diagram";
/// A diagram never grows past this height; wider ones shrink to the column.
const MAX_HEIGHT: f32 = 480.0;
const COPIED_FOR: Duration = Duration::from_secs(2);
/// Diagrams kept per process, as the web's 100-entry render cache.
const CACHE_LIMIT: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Diagram {
    source: SharedString,
}

#[derive(Clone)]
enum DiagramState {
    Pending,
    Ready {
        image: Arc<Image>,
        width: f32,
        height: f32,
    },
    Failed,
}

type CacheKey = (SharedString, String);

thread_local! {
    // Diagrams draw while the window draws, on the main thread.
    static CACHE: RefCell<HashMap<CacheKey, DiagramState>> = RefCell::new(HashMap::new());
    static COPIED: RefCell<Option<SharedString>> = const { RefCell::new(None) };
}

pub(crate) struct MarkdownDiagram;

impl MarkdownPlugin for MarkdownDiagram {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        DIAGRAM
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown_ast::Node::Code(code) = node else {
            return None;
        };
        if code.lang.as_deref() != Some("mermaid") {
            return None;
        }
        Some(
            MarkdownNode::new(
                DIAGRAM,
                Diagram {
                    source: code.value.clone().into(),
                },
            )
            .text(code.value.clone())
            .markdown(cx.node_source(node).unwrap_or_default().to_owned()),
        )
    }

    fn render(&self, node: &MarkdownNode, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Some(diagram) = node.data::<Diagram>() else {
            return div().into_any_element();
        };
        let palette = cx.global::<Palette>().clone();
        let font = Theme::global(cx).font_family.to_string();
        let key = (diagram.source.clone(), palette.id.clone());
        let state = CACHE.with_borrow(|cache| cache.get(&key).cloned());
        let state = match state {
            Some(state) => state,
            None => {
                start_render(key.clone(), mermaid_theme(&palette, font), cx);
                DiagramState::Pending
            }
        };
        let copied = COPIED.with_borrow(|copied| copied.as_ref() == Some(&diagram.source));
        let strings = locale::strings(cx);
        let copy_title = strings.get(if copied {
            "markdownRenderer.code.actions.copiedTitle"
        } else {
            "markdownRenderer.mermaid.actions.copyTitle"
        });
        let source = diagram.source.clone();
        let copy = action_button(
            format!("mermaid-copy-{}", short_hash(&diagram.source)).into(),
            if copied { "check" } else { "file-copy" },
            copy_title,
            &palette,
            move |window, cx| copy_source(source.clone(), window, cx),
        )
        .test_support();
        let body = match state {
            DiagramState::Ready {
                image,
                width,
                height,
            } => div()
                .w_full()
                .flex()
                .justify_center()
                .child(
                    img(image)
                        .max_w_full()
                        .w(px(width))
                        .h(px(height.min(MAX_HEIGHT)))
                        .flex_shrink_1(),
                )
                .into_any_element(),
            DiagramState::Pending | DiagramState::Failed => {
                let mono = Theme::global(cx).mono_font_family.clone();
                div()
                    .font_family(mono)
                    .text_size(rems(0.8125))
                    .text_color(palette.muted_foreground)
                    .whitespace_normal()
                    .child(diagram.source.clone())
                    .into_any_element()
            }
        };
        div()
            .relative()
            .w_full()
            .p_3()
            .rounded(px(12.0))
            .border_1()
            .border_color(palette.border.opacity(0.7))
            .child(body)
            .child(div().absolute().top_1().right_1().child(copy))
            .into_any_element()
    }
}

fn copy_source(source: SharedString, window: &mut Window, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(source.to_string()));
    COPIED.with_borrow_mut(|copied| *copied = Some(source.clone()));
    window.refresh();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(COPIED_FOR).await;
        cx.update(|cx| {
            COPIED.with_borrow_mut(|copied| {
                if copied.as_ref() == Some(&source) {
                    *copied = None;
                }
            });
            cx.refresh_windows();
        });
    })
    .detach();
}

/// Lays the diagram out on the background executor and redraws once done.
fn start_render(key: CacheKey, theme: mermaid_rs_renderer::Theme, cx: &mut App) {
    CACHE.with_borrow_mut(|cache| {
        if cache.len() >= CACHE_LIMIT {
            cache.retain(|_, state| matches!(state, DiagramState::Pending));
        }
        cache.insert(key.clone(), DiagramState::Pending);
    });
    let source = key.0.to_string();
    let task = cx
        .background_executor()
        .spawn(async move { render_diagram(&source, theme) });
    cx.spawn(async move |cx| {
        let state = task.await;
        cx.update(|cx| {
            CACHE.with_borrow_mut(|cache| cache.insert(key, state));
            cx.refresh_windows();
        });
    })
    .detach();
}

fn render_diagram(source: &str, theme: mermaid_rs_renderer::Theme) -> DiagramState {
    let options = mermaid_rs_renderer::RenderOptions {
        theme,
        ..mermaid_rs_renderer::RenderOptions::default()
    };
    let Ok(svg) = mermaid_rs_renderer::render_strict(source, options) else {
        return DiagramState::Failed;
    };
    let Some((width, height)) = svg_size(&svg) else {
        return DiagramState::Failed;
    };
    DiagramState::Ready {
        image: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())),
        width,
        height,
    }
}

/// The root element's `width` and `height` in pixels.
fn svg_size(svg: &str) -> Option<(f32, f32)> {
    let root = &svg[svg.find("<svg")?..];
    let root = &root[..root.find('>')?];
    let attribute = |name: &str| -> Option<f32> {
        let start = root.find(&format!(" {name}=\""))? + name.len() + 3;
        let value = &root[start..];
        value[..value.find('"')?]
            .trim_end_matches("px")
            .parse()
            .ok()
    };
    Some((attribute("width")?, attribute("height")?))
}

/// The theme's colors for nodes, edges and text (`screenColors` in
/// `MarkdownMermaid.tsx`), on a transparent background.
fn mermaid_theme(palette: &Palette, font: String) -> mermaid_rs_renderer::Theme {
    let hex = |color: Hsla| {
        let rgba = Rgba::from(color);
        format!(
            "#{:02x}{:02x}{:02x}",
            (rgba.r * 255.0).round() as u8,
            (rgba.g * 255.0).round() as u8,
            (rgba.b * 255.0).round() as u8
        )
    };
    let mut theme = match palette.appearance {
        crate::theme::Appearance::Dark => mermaid_rs_renderer::Theme::dark(),
        crate::theme::Appearance::Light => mermaid_rs_renderer::Theme::modern(),
    };
    theme.font_family = font;
    theme.primary_color = hex(palette.muted);
    theme.primary_text_color = hex(palette.foreground);
    theme.primary_border_color = hex(palette.border);
    theme.line_color = hex(palette.muted_foreground);
    theme.secondary_color = hex(palette.elevated);
    theme.tertiary_color = hex(palette.subtle);
    theme.edge_label_background = hex(palette.chat_background);
    theme.cluster_background = hex(palette.subtle);
    theme.cluster_border = hex(palette.border);
    theme.background = "transparent".to_owned();
    theme.text_color = hex(palette.foreground);
    theme
}

fn short_hash(source: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagrams_lay_out_and_broken_sources_fail() {
        let theme = mermaid_rs_renderer::Theme::modern();
        let DiagramState::Ready { width, height, .. } = render_diagram(
            "graph TD\n  A[Start] --> B{Check}\n  B -->|yes| C[Done]",
            theme.clone(),
        ) else {
            panic!("a flowchart lays out");
        };
        assert!(width > 0.0 && height > 0.0);
        assert!(matches!(
            render_diagram("graph TD\n  A -->", theme),
            DiagramState::Failed
        ));
    }

    #[test]
    fn reads_the_root_size() {
        assert_eq!(
            svg_size(
                r#"<svg xmlns="x" width="120" height="48.5px" viewBox="0 0 120 48"><g/></svg>"#
            ),
            Some((120.0, 48.5))
        );
        assert_eq!(svg_size("<g/>"), None);
    }
}
