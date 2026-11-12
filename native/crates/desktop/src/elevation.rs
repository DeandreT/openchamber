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

/// `.oc-glass-floating.oc-composer-sheen`: `--oc-elevation-raised`, one level
/// below the menus that open over the composer.
///
/// The recipe's rings are half a pixel wide. At scale factor 1 Chromium
/// paints the dark inset ring as a one-pixel line at half its alpha, which is
/// what the inset ring here draws. The outer rings keep the one-pixel alphas
/// [`floating`] uses for the same `0.5px` rings.
pub fn composer(palette: &Palette) -> Vec<BoxShadow> {
    let mut shadows = match palette.appearance {
        Appearance::Dark => vec![
            shadow(1.0, 0.0, 0.0, white(0.04), true),
            shadow(0.0, 0.0, 1.0, white(0.04), true),
            shadow(0.0, 0.0, 1.0, black(0.36), false),
            shadow(2.0, 4.0, 0.0, black(0.25), false),
            shadow(1.0, 2.0, 0.0, black(0.25), false),
        ],
        Appearance::Light => vec![
            shadow(1.0, 0.0, 0.0, white(0.7), true),
            shadow(0.0, 0.0, 1.0, black(0.1), false),
            shadow(1.0, 2.0, -0.5, black(0.06), false),
            shadow(4.0, 10.0, -2.0, black(0.07), false),
        ],
    };
    shadows.reverse();
    shadows
}

/// `.oc-glass-floating`: the shared popup edge and shadow.
pub fn floating(palette: &Palette) -> Vec<BoxShadow> {
    floating_surface(palette, 0.06, 0.8)
}

/// Builds the `.oc-raised` control's inset edge, top light and short shadow.
pub fn control(palette: &Palette) -> Vec<BoxShadow> {
    let mut shadows = match palette.appearance {
        Appearance::Light => vec![
            shadow(0.0, 0.0, 1.0, palette.foreground.opacity(0.1), true),
            shadow(1.0, 2.0, 0.0, black(0.06), false),
        ],
        Appearance::Dark => vec![
            shadow(1.0, 0.0, 0.0, palette.foreground.opacity(0.015), true),
            shadow(0.0, 0.0, 1.0, palette.foreground.opacity(0.06), true),
            shadow(1.0, 1.0, 0.0, black(0.28), false),
            shadow(2.0, 3.0, -2.0, black(0.2), false),
        ],
    };
    shadows.reverse();
    shadows
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
    fn controls_follow_the_short_raised_shadow_recipe() {
        let light = control(&Palette::default_for(Appearance::Light));
        assert_eq!(light.len(), 2);
        assert_eq!(light[0].offset.y, px(1.0));
        assert_eq!(light[0].blur_radius, px(1.0));
        assert!(light[1].inset);
        assert_eq!(light[1].color.a, 0.1);
        let dark = control(&Palette::default_for(Appearance::Dark));
        assert_eq!(dark.len(), 4);
        assert_eq!(dark[0].spread_radius, px(-2.0));
        assert_eq!(dark[3].color.a, 0.015);
        assert!(dark[3].inset);
    }

    #[test]
    fn css_recipes_convert_blur_and_paint_top_shadow_last() {
        let palette = Palette::default_for(Appearance::Light);
        let shadows = floating(&palette);
        assert_eq!(shadows[0].blur_radius, px(10.0));
        assert_eq!(shadows[0].offset.y, px(12.0));
        let highlight = shadows.last().expect("a popup includes its top highlight");
        assert!(highlight.inset);
        assert_eq!(highlight.color.a, 0.8);
        assert_eq!(panel_edge(&palette)[0].blur_radius, px(4.0));
    }

    #[test]
    fn the_composer_takes_the_raised_level() {
        let dark = composer(&Palette::default_for(Appearance::Dark));
        assert_eq!(dark.len(), 5, "top light, two rings and two shadows");
        // `0 1px 2px 0` and `0 2px 4px 0`, both at a quarter black.
        assert_eq!(
            (dark[0].offset.y, dark[0].blur_radius, dark[0].spread_radius),
            (px(1.0), px(1.0), px(0.0))
        );
        assert_eq!(
            (dark[1].offset.y, dark[1].blur_radius, dark[1].spread_radius),
            (px(2.0), px(2.0), px(0.0))
        );
        assert_eq!((dark[0].color.a, dark[1].color.a), (0.25, 0.25));
        assert!(!dark[2].inset, "the dark ring outside the box");
        let ring = &dark[3];
        assert!(ring.inset);
        assert_eq!(
            (ring.spread_radius, ring.color.a),
            (px(1.0), 0.04),
            "half of the 0.5px ring's 8% white on one pixel"
        );
        let top_light = &dark[4];
        assert!(top_light.inset);
        assert_eq!((top_light.offset.y, top_light.color.a), (px(1.0), 0.04));

        let light = composer(&Palette::default_for(Appearance::Light));
        assert_eq!(light.len(), 4, "top light, one ring and two shadows");
        // `0 4px 10px -2px` at 7% black is the lowest layer.
        assert_eq!(
            (
                light[0].offset.y,
                light[0].blur_radius,
                light[0].spread_radius
            ),
            (px(4.0), px(5.0), px(-2.0))
        );
        assert_eq!(light[0].color.a, 0.07);
        assert_eq!(
            light.iter().filter(|shadow| shadow.inset).count(),
            1,
            "the light recipe has no inset ring"
        );
        assert_eq!(light[3].color.a, 0.7);
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
