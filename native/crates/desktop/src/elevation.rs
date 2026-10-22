//! Elevation recipes of `packages/ui/src/styles/design-system.css`.
//!
//! The web client fixes these rings and shadows per light or dark appearance
//! rather than per theme, as black and white alphas or a mix of the theme's
//! foreground. Each function names the rule it mirrors. CSS lists the topmost
//! shadow first; GPUI paints in the same order, inset shadows included.

use gpui_kit::{BoxShadow, Hsla, hsla, point, px};

use crate::theme::{Appearance, Palette};

fn shadow(y: f32, blur: f32, spread: f32, color: Hsla, inset: bool) -> BoxShadow {
    BoxShadow {
        color,
        offset: point(px(0.0), px(y)),
        blur_radius: px(blur),
        spread_radius: px(spread),
        inset,
    }
}

fn white(alpha: f32) -> Hsla {
    hsla(0.0, 0.0, 1.0, alpha)
}

fn black(alpha: f32) -> Hsla {
    hsla(0.0, 0.0, 0.0, alpha)
}

/// The desktop composer box: `.oc-glass-floating`, and in dark themes
/// `.dark .oc-glass-floating.oc-composer-sheen`.
pub fn composer(palette: &Palette) -> Vec<BoxShadow> {
    match palette.appearance {
        Appearance::Dark => vec![
            shadow(1.0, 0.0, 0.0, white(0.025), true),
            shadow(0.0, 0.0, 1.0, white(0.08), true),
            shadow(0.0, 0.0, 1.0, black(0.36), false),
            shadow(1.0, 1.0, -0.5, black(0.22), false),
            shadow(3.0, 3.0, -1.5, black(0.2), false),
            shadow(6.0, 6.0, -3.0, black(0.16), false),
        ],
        Appearance::Light => vec![
            shadow(1.0, 0.0, 0.0, white(0.8), true),
            shadow(0.0, 0.0, 1.0, black(0.04), true),
            shadow(0.0, 0.0, 1.0, black(0.1), false),
            shadow(1.0, 2.0, -0.5, black(0.08), false),
            shadow(4.0, 8.0, -2.0, black(0.08), false),
            shadow(12.0, 20.0, -4.0, black(0.08), false),
        ],
    }
}

/// The dark composer's sheen, `color-mix(foreground 1%)` fading to nothing
/// halfway down; light themes keep the plain fill.
pub fn composer_sheen(palette: &Palette) -> Option<Hsla> {
    (palette.appearance == Appearance::Dark).then(|| palette.foreground.opacity(0.01))
}

/// The outer shadows of a docked `.oc-panel-edge` card.
pub fn panel_edge(palette: &Palette) -> Vec<BoxShadow> {
    match palette.appearance {
        Appearance::Dark => vec![
            shadow(1.0, 1.0, 0.0, black(0.35), false),
            shadow(4.0, 8.0, -3.0, black(0.45), false),
        ],
        Appearance::Light => vec![
            shadow(1.0, 2.0, 0.0, black(0.06), false),
            shadow(4.0, 8.0, -3.0, black(0.12), false),
        ],
    }
}

/// The ring `.oc-panel-edge::after` draws above the card's content.
pub fn panel_edge_ring(palette: &Palette) -> Vec<BoxShadow> {
    match palette.appearance {
        Appearance::Dark => vec![
            shadow(1.0, 0.0, 0.0, palette.foreground.opacity(0.03), true),
            shadow(0.0, 0.0, 1.0, palette.foreground.opacity(0.06), true),
        ],
        Appearance::Light => vec![shadow(0.0, 0.0, 1.0, black(0.12), true)],
    }
}

/// The fill of a docked panel card: the sidebar's muted surface in dark
/// themes, halfway between it and the chat background in light ones.
pub fn panel_fill(palette: &Palette) -> Hsla {
    match palette.appearance {
        Appearance::Dark => palette.muted,
        Appearance::Light => mix(palette.muted, palette.chat_background, 0.5),
    }
}

/// `color-mix(in srgb, a weight, b)`.
pub fn mix(a: Hsla, b: Hsla, weight: f32) -> Hsla {
    let (a, b) = (a.to_rgb(), b.to_rgb());
    let channel = |x: f32, y: f32| x * weight + y * (1.0 - weight);
    Hsla::from(gpui_kit::Rgba {
        r: channel(a.r, b.r),
        g: channel(a.g, b.g),
        b: channel(a.b, b.b),
        a: channel(a.a, b.a),
    })
}
