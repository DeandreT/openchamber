//! Text on the baseline Chromium gives it.
//!
//! Both clients center a font's ascent and descent in the line box, but
//! Chromium rounds the ascent and descent to whole pixels first and floors the
//! half-leading, while GPUI centers the exact metrics and rounds the glyph
//! origin to the nearest pixel, ties down. The same text in the same line box
//! can therefore sit a pixel apart. [`CssTextExt::css_text`] moves a text
//! element's glyphs by that difference, computed from the metrics of the font
//! the text actually uses, at the scale factor 1 the parity runs use.

use std::cell::RefCell;
use std::collections::HashMap;

use gpui_kit::component::Theme;
use gpui_kit::{App, SharedString, Styled, font, px, rems};

/// Native views keep GPUI's root font size, the web's 16 px root.
const ROOT_FONT_SIZE: f32 = 16.0;

/// Chromium's baseline minus GPUI's, in pixels, for a font with `ascent` and
/// `descent` (both positive, in pixels) in a line box of `line_height`.
///
/// Chromium: `round(ascent) + floor((line_height - round(ascent) -
/// round(descent)) / 2)`, as `FontHeight::AddLeading` floors the
/// half-leading. GPUI: `(line_height - ascent - descent) / 2 + ascent`,
/// rounded half toward zero by its glyph quantization.
pub(crate) fn baseline_shift(ascent: f32, descent: f32, line_height: f32) -> f32 {
    let (whole_ascent, whole_descent) = (ascent.round(), descent.round());
    let chromium = whole_ascent + ((line_height - whole_ascent - whole_descent) / 2.0).floor();
    let gpui = (line_height - ascent - descent) / 2.0 + ascent;
    let gpui = if gpui - gpui.floor() > 0.5 {
        gpui.ceil()
    } else {
        gpui.floor()
    };
    chromium - gpui
}

/// Shifts already computed, by UI family, size and line height.
type ShiftKey = (SharedString, u32, u32);

thread_local! {
    static SHIFTS: RefCell<HashMap<ShiftKey, f32>> = RefCell::new(HashMap::new());
}

/// The shift for the UI font at `size` in a line of `line_height`, in pixels.
fn ui_font_shift(size: f32, line_height: f32, cx: &App) -> f32 {
    let family = Theme::global(cx).font_family.clone();
    let key = (family.clone(), size.to_bits(), line_height.to_bits());
    if let Some(shift) = SHIFTS.with(|shifts| shifts.borrow().get(&key).copied()) {
        return shift;
    }
    let text_system = cx.text_system();
    let font_id = text_system.resolve_font(&font(family));
    let ascent = f32::from(text_system.ascent(font_id, px(size)));
    // Font metrics give the descent below the baseline as a negative
    // offset; line layout, which places the glyphs, uses its magnitude.
    let descent = f32::from(text_system.descent(font_id, px(size))).abs();
    let shift = baseline_shift(ascent, descent, line_height);
    tracing::debug!(family = %key.0, size, line_height, ascent, descent, shift, "text baseline shift");
    SHIFTS.with(|shifts| shifts.borrow_mut().insert(key, shift));
    shift
}

/// Text sizing that lands on Chromium's baseline.
pub(crate) trait CssTextExt: Styled + Sized {
    /// Sets the text size and line height in rems and moves the glyphs onto
    /// Chromium's baseline for that line. The move is a relative offset, so
    /// use it on elements that hold only text: it moves the whole element.
    fn css_text(self, size: f32, line_height: f32, cx: &App) -> Self {
        let shift = ui_font_shift(size * ROOT_FONT_SIZE, line_height * ROOT_FONT_SIZE, cx);
        let sized = self.text_size(rems(size)).line_height(rems(line_height));
        if shift == 0.0 {
            sized
        } else {
            sized.relative().top(px(shift))
        }
    }
}

impl<T: Styled> CssTextExt for T {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Noto Sans: ascent 1.069 em, descent 0.293 em. The shifts match the
    /// offsets measured between the Electron and native captures.
    #[test]
    fn shifts_follow_chromium_rounding() {
        let noto =
            |size: f32, line_height: f32| baseline_shift(1.069 * size, 0.293 * size, line_height);
        assert_eq!(noto(12.0, 12.0), -1.0, "a compact age");
        assert_eq!(noto(12.5, 18.0), -1.0, "typography-meta");
        assert_eq!(noto(30.0, 36.0), -1.0, "the startup heading");
        assert_eq!(noto(12.0, 16.0), -1.0, "the work status heading");
        assert_eq!(noto(13.5, 20.0), 0.0, "a sidebar title");
        assert_eq!(noto(14.0, 20.0), 0.0, "a zone header");
        assert_eq!(noto(14.0, 17.5), 0.0, "the title bar");
        assert_eq!(noto(12.0, 15.0), 0.0, "a group label");
    }
}
