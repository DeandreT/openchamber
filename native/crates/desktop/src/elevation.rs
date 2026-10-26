//! Elevation recipes of `packages/ui/src/styles/design-system.css`.
//!
//! The web client fixes these rings and shadows per light or dark appearance
//! rather than per theme, as black and white alphas or a mix of the theme's
//! foreground. Each function names the rule it mirrors. CSS lists the topmost
//! shadow first; each recipe reverses it for GPUI's back-to-front painter.

use gpui_kit::{BoxShadow, Hsla, hsla, point, px};

use crate::locale::Direction;
use crate::theme::{Appearance, Palette};

fn shadow(y: f32, blur: f32, spread: f32, color: Hsla, inset: bool) -> BoxShadow {
    BoxShadow {
        color,
        offset: point(px(0.0), px(y)),
        // GPUI's shader takes Gaussian sigma; CSS specifies twice that value.
        blur_radius: px(blur * 0.5),
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

/// The shared floating shadow stack; the dark top light depends on its surface.
fn floating_surface(
    palette: &Palette,
    dark_top_light: f32,
    light_top_light: f32,
) -> Vec<BoxShadow> {
    let mut shadows = match palette.appearance {
        Appearance::Dark => vec![
            shadow(1.0, 0.0, 0.0, white(dark_top_light), true),
            shadow(0.0, 0.0, 1.0, white(0.08), true),
            shadow(0.0, 0.0, 1.0, black(0.36), false),
            shadow(1.0, 1.0, -0.5, black(0.22), false),
            shadow(3.0, 3.0, -1.5, black(0.2), false),
            shadow(6.0, 6.0, -3.0, black(0.16), false),
        ],
        Appearance::Light => vec![
            shadow(1.0, 0.0, 0.0, white(light_top_light), true),
            shadow(0.0, 0.0, 1.0, black(0.04), true),
            shadow(0.0, 0.0, 1.0, black(0.1), false),
            shadow(1.0, 2.0, -0.5, black(0.08), false),
            shadow(4.0, 8.0, -2.0, black(0.08), false),
            shadow(12.0, 20.0, -4.0, black(0.08), false),
        ],
    };
    shadows.reverse();
    shadows
}

/// `.oc-glass-floating.oc-composer-sheen` uses a quieter dark top light.
pub fn composer(palette: &Palette) -> Vec<BoxShadow> {
    floating_surface(palette, 0.025, 0.55)
}

/// `.oc-glass-floating`: the shared popup edge and shadow.
pub fn floating(palette: &Palette) -> Vec<BoxShadow> {
    floating_surface(palette, 0.06, 0.8)
}

/// The dark composer's sheen, `color-mix(foreground 1%)` fading to nothing
/// halfway down; light themes keep the plain fill.
pub fn composer_sheen(palette: &Palette) -> Option<Hsla> {
    (palette.appearance == Appearance::Dark).then(|| palette.foreground.opacity(0.01))
}

/// The outer shadows of a docked `.oc-panel-edge` card.
pub fn panel_edge(palette: &Palette) -> Vec<BoxShadow> {
    let mut shadows = match palette.appearance {
        Appearance::Dark => vec![
            shadow(1.0, 1.0, 0.0, black(0.35), false),
            shadow(4.0, 8.0, -3.0, black(0.45), false),
        ],
        Appearance::Light => vec![
            shadow(1.0, 2.0, 0.0, black(0.06), false),
            shadow(4.0, 8.0, -3.0, black(0.12), false),
        ],
    };
    shadows.reverse();
    shadows
}

/// The ring `.oc-panel-edge::after` draws above the card's content.
pub fn panel_edge_ring(palette: &Palette) -> Vec<BoxShadow> {
    let mut shadows = match palette.appearance {
        Appearance::Dark => vec![
            shadow(1.0, 0.0, 0.0, palette.foreground.opacity(0.03), true),
            shadow(0.0, 0.0, 1.0, palette.foreground.opacity(0.06), true),
        ],
        Appearance::Light => vec![shadow(0.0, 0.0, 1.0, black(0.12), true)],
    };
    shadows.reverse();
    shadows
}

/// The sidebar's inward shadow along the edge facing the chat.
pub fn sidebar(direction: Direction) -> Vec<BoxShadow> {
    let mut edge = shadow(0.0, 10.0, -2.0, black(0.06), true);
    edge.offset.x = px(if direction == Direction::RightToLeft {
        2.0
    } else {
        -2.0
    });
    vec![edge]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_recipes_convert_blur_and_paint_top_shadow_last() {
        let palette = Palette::default_for(Appearance::Light);
        let shadows = composer(&palette);
        assert_eq!(shadows[0].blur_radius, px(10.0));
        assert_eq!(shadows[0].offset.y, px(12.0));
        let highlight = shadows.last().expect("composer includes its top highlight");
        assert!(highlight.inset);
        assert_eq!(highlight.color.a, 0.55);
        assert_eq!(panel_edge(&palette)[0].blur_radius, px(4.0));
    }

    #[test]
    fn sidebar_shadow_follows_the_chat_edge() {
        let left = sidebar(Direction::LeftToRight);
        let right = sidebar(Direction::RightToLeft);
        assert_eq!(left[0].offset.x, -right[0].offset.x);
        assert_eq!(left[0].blur_radius, px(5.0));
        assert!(left[0].inset && right[0].inset);
    }
}
