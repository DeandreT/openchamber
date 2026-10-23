//! Text with CSS `letter-spacing`, which GPUI's text elements lack.
//!
//! The line is shaped as usual, keeping the font's kerning, and every glyph
//! moves by the spacing of the characters before it: Chromium adds the
//! spacing after each character, the last one included, so the line's width
//! grows by the spacing once per character. Combining marks count as
//! characters. Text that does not fit on one line, or that holds
//! right-to-left characters (whose cursive joins spacing would break), is
//! laid out as ordinary wrapped text without the spacing.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::{
    App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, LineLayout, Pixels, SharedString, Size, Style, TextAlign, Window,
    WrappedLine, point, size,
};

/// `text` with `letter_spacing` in ems, aligned in its box like `text-align`.
pub(crate) fn tracked_text(
    text: impl Into<SharedString>,
    letter_spacing: f32,
    align: TextAlign,
) -> TrackedText {
    TrackedText {
        text: text.into(),
        letter_spacing,
        align,
        metrics: None,
    }
}

pub(crate) struct TrackedText {
    text: SharedString,
    letter_spacing: f32,
    align: TextAlign,
    /// The inherited text style, read during layout.
    metrics: Option<Metrics>,
}

#[derive(Clone, Copy)]
struct Metrics {
    font_size: Pixels,
    line_height: Pixels,
    color: Hsla,
}

/// How the measured text is drawn.
pub(crate) enum Shaped {
    /// One line with the spacing applied.
    Tracked {
        layout: Arc<LineLayout>,
        spacing: Pixels,
        width: Pixels,
    },
    /// Wrapped lines without spacing.
    Wrapped(Vec<WrappedLine>),
}

impl IntoElement for TrackedText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TrackedText {
    type RequestLayoutState = Rc<RefCell<Option<Shaped>>>;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = window.text_style();
        let rem_size = window.rem_size();
        let metrics = Metrics {
            font_size: style.font_size.to_pixels(rem_size),
            line_height: style.line_height_in_pixels(rem_size),
            color: style.color,
        };
        self.metrics = Some(metrics);
        let run = style.to_run(self.text.len());
        let spacing = metrics.font_size * self.letter_spacing;
        let text = self.text.clone();
        let shaped = Rc::new(RefCell::new(None));
        let measured = shaped.clone();
        let layout_id = window.request_measured_layout(
            Style::default(),
            move |known, available, window, _cx| {
                let limit = known.width.or(match available.width {
                    AvailableSpace::Definite(width) => Some(width),
                    AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
                });
                let runs = [run.clone()];
                let layout =
                    window
                        .text_system()
                        .layout_line(&text, metrics.font_size, &runs, None);
                let width = layout.width + spacing * text.chars().count() as f32;
                if limit.is_none_or(|limit| width <= limit) && !has_right_to_left(&text) {
                    *measured.borrow_mut() = Some(Shaped::Tracked {
                        layout,
                        spacing,
                        width,
                    });
                    return size(width, metrics.line_height);
                }
                let lines: Vec<WrappedLine> = window
                    .text_system()
                    .shape_text(text.clone(), metrics.font_size, &runs, limit, None)
                    .map(|lines| lines.into_iter().collect())
                    .unwrap_or_default();
                let mut extent: Size<Pixels> = Size::default();
                for line in &lines {
                    let line_size = line.size(metrics.line_height);
                    extent.width = extent.width.max(line_size.width);
                    extent.height += line_size.height;
                }
                *measured.borrow_mut() = Some(Shaped::Wrapped(lines));
                extent
            },
        );
        (layout_id, shaped)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _shaped: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        shaped: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(metrics) = self.metrics else {
            return;
        };
        let shaped = shaped.borrow();
        match shaped.as_ref() {
            Some(Shaped::Tracked {
                layout,
                spacing,
                width,
            }) => {
                let start = match self.align {
                    TextAlign::Left => bounds.left(),
                    TextAlign::Center => bounds.left() + (bounds.size.width - *width) / 2.0,
                    TextAlign::Right => bounds.right() - *width,
                };
                // GPUI's own placement, so `css_text` corrects both alike.
                let baseline = bounds.top()
                    + (metrics.line_height - layout.ascent - layout.descent) / 2.0
                    + layout.ascent;
                let character_starts: Vec<usize> =
                    self.text.char_indices().map(|(index, _)| index).collect();
                for run in &layout.runs {
                    for glyph in &run.glyphs {
                        let before = character_starts.partition_point(|&start| start < glyph.index);
                        let origin = point(
                            start + glyph.position.x + *spacing * before as f32,
                            baseline + glyph.position.y,
                        );
                        let painted = if glyph.is_emoji {
                            window.paint_emoji(origin, run.font_id, glyph.id, metrics.font_size)
                        } else {
                            window.paint_glyph(
                                origin,
                                run.font_id,
                                glyph.id,
                                metrics.font_size,
                                metrics.color,
                            )
                        };
                        if let Err(error) = painted {
                            tracing::debug!(%error, "a glyph could not be painted");
                        }
                    }
                }
            }
            Some(Shaped::Wrapped(lines)) => {
                let mut origin = bounds.origin;
                for line in lines {
                    if let Err(error) = line.paint(
                        origin,
                        metrics.line_height,
                        self.align,
                        Some(bounds),
                        window,
                        cx,
                    ) {
                        tracing::debug!(%error, "a line could not be painted");
                    }
                    origin.y += line.size(metrics.line_height).height;
                }
            }
            None => {}
        }
    }
}

/// Whether `text` holds a character of a right-to-left script.
fn has_right_to_left(text: &str) -> bool {
    text.chars().any(|character| {
        matches!(
            u32::from(character),
            0x0590..=0x08ff | 0xfb1d..=0xfdff | 0xfe70..=0xfeff | 0x10800..=0x10fff | 0x1e800..=0x1efff
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_to_left_scripts_are_recognized() {
        assert!(has_right_to_left("ما الذي نعمل عليه؟"));
        assert!(has_right_to_left("על מה אנחנו עובדים?"));
        assert!(!has_right_to_left("What are we working on?"));
        assert!(!has_right_to_left("Над чим ми працюємо?"));
    }
}
