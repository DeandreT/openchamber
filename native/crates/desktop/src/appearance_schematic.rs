//! Desktop drawing from `ThemeSchematic.tsx`, in its physical 240 by 150 space.
//! The drawing stays left-to-right in every locale, as the reference SVG does.

use std::fmt::Write;
use std::sync::Arc;

use gpui_kit::{Hsla, Image, ImageFormat};

use crate::elevation;
use crate::theme::{Appearance, Palette};

pub(super) fn image(theme: &Palette, split: Option<&Palette>) -> Arc<Image> {
    let mut svg = String::from(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="240" height="150" viewBox="0 0 240 150">"#,
    );
    draw(&mut svg, theme, "base");
    if let Some(split) = split {
        svg.push_str(r#"<defs><clipPath id="split"><polygon points="0,0 148.8,0 91.2,150 0,150"/></clipPath></defs><g clip-path="url(#split)">"#);
        draw(&mut svg, split, "light");
        svg.push_str("</g>");
    }
    svg.push_str("</svg>");
    Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes()))
}

fn color(color: Hsla) -> String {
    let color = color.to_rgb();
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        (color.r * 255.0).round() as u8,
        (color.g * 255.0).round() as u8,
        (color.b * 255.0).round() as u8,
        (color.a * 255.0).round() as u8,
    )
}

fn rect(svg: &mut String, x: f32, y: f32, w: f32, h: f32, r: f32, fill: Hsla) {
    let _ = write!(
        svg,
        r#"<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{r}" fill="{}"/>"#,
        color(fill)
    );
}

fn bar(svg: &mut String, x: f32, y: f32, w: f32, h: f32, fill: Hsla) {
    rect(svg, x, y, w, h, h / 2.0, fill);
}

fn dot(svg: &mut String, x: f32, y: f32, r: f32, fill: Hsla) {
    let _ = write!(
        svg,
        r#"<circle cx="{x}" cy="{y}" r="{r}" fill="{}"/>"#,
        color(fill)
    );
}

fn ring(svg: &mut String, x: f32, y: f32, r: f32, stroke: Hsla, width: f32) {
    let _ = write!(
        svg,
        r#"<circle cx="{x}" cy="{y}" r="{r}" fill="none" stroke="{}" stroke-width="{width}"/>"#,
        color(stroke)
    );
}

fn edge(svg: &mut String, x: f32, y: f32, w: f32, h: f32, r: f32, stroke: Hsla) {
    let _ = write!(
        svg,
        r#"<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{r}" fill="none" stroke="{}" stroke-width="0.5"/>"#,
        color(stroke)
    );
}

fn code(svg: &mut String, x: f32, y: f32, w: f32, p: &Palette) {
    rect(
        svg,
        x - 2.0,
        y - 2.0,
        w + 4.0,
        8.5,
        2.5,
        p.inline_code_background.opacity(0.5),
    );
    rect(
        svg,
        x - 2.0,
        y - 2.0,
        w + 4.0,
        8.5,
        2.5,
        p.foreground.opacity(0.04),
    );
    bar(svg, x, y, w, 4.5, p.inline_code.opacity(0.9));
}

/// `color-mix(in oklab, foreground 50%, mutedForeground)` for resting titles.
fn title_rest(p: &Palette) -> Hsla {
    fn lab(color: Hsla) -> [f32; 3] {
        let rgb = color.to_rgb();
        let linear = |v: f32| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let (r, g, b) = (linear(rgb.r), linear(rgb.g), linear(rgb.b));
        let l = (0.41222146 * r + 0.53633255 * g + 0.051445995 * b).cbrt();
        let m = (0.2119035 * r + 0.6806995 * g + 0.10739696 * b).cbrt();
        let s = (0.08830246 * r + 0.28171885 * g + 0.6299787 * b).cbrt();
        [
            0.21045426 * l + 0.7936178 * m - 0.004072047 * s,
            1.9779985 * l - 2.4285922 * m + 0.4505937 * s,
            0.025904037 * l + 0.78277177 * m - 0.80867577 * s,
        ]
    }
    let a = lab(p.foreground);
    let b = lab(p.muted_foreground);
    let [l, a, b] = std::array::from_fn::<_, 3, _>(|i| (a[i] + b[i]) / 2.0);
    let x = (l + 0.39633778 * a + 0.21580376 * b).powi(3);
    let y = (l - 0.105561346 * a - 0.06385417 * b).powi(3);
    let z = (l - 0.08948418 * a - 1.2914855 * b).powi(3);
    let srgb = |v: f32| {
        (if v <= 0.0031308 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        })
        .clamp(0.0, 1.0)
    };
    gpui_kit::Rgba {
        r: srgb(4.0767417 * x - 3.3077116 * y + 0.23096994 * z),
        g: srgb(-1.268438 * x + 2.6097574 * y - 0.3413194 * z),
        b: srgb(-0.0041960863 * x - 0.7034186 * y + 1.7076147 * z),
        a: 1.0,
    }
    .into()
}

fn draw(svg: &mut String, p: &Palette, gradient: &str) {
    let dark = p.appearance == Appearance::Dark;
    let text = p.foreground.opacity(0.9);
    let edge_color = gpui_kit::hsla(
        0.0,
        0.0,
        if dark { 1.0 } else { 0.0 },
        if dark { 0.08 } else { 0.14 },
    );
    rect(svg, 0.0, 0.0, 240.0, 150.0, 0.0, p.background);
    rect(svg, 0.0, 0.0, 64.0, 150.0, 0.0, p.muted);
    rect(svg, 63.5, 0.0, 0.5, 150.0, 0.0, p.border);
    bar(svg, 8.0, 14.0, 20.0, 4.0, p.muted_foreground);
    for (y, w, mark, selected) in [
        (27.0, 32.0, 1, false),
        (41.0, 26.0, 2, false),
        (55.0, 36.0, 0, false),
        (87.0, 30.0, 0, false),
        (101.0, 28.0, 3, true),
        (115.0, 36.0, 4, false),
        (129.0, 24.0, 0, false),
    ] {
        if selected {
            rect(svg, 4.0, y - 5.0, 56.0, 15.0, 4.0, p.selection.opacity(0.7));
        }
        bar(
            svg,
            11.0,
            y,
            w,
            4.5,
            if selected {
                p.selection_foreground
            } else if mark == 2 {
                p.foreground
            } else {
                title_rest(p)
            },
        );
        match mark {
            1 => ring(svg, 53.0, y + 2.25, 2.8, p.status_info, 1.3),
            2 => dot(svg, 53.0, y + 2.25, 3.0, p.status_success),
            3 | 4 => rect(
                svg,
                50.0,
                y - 0.75,
                6.0,
                6.0,
                1.6,
                if mark == 3 { p.pr_merged } else { p.pr_open },
            ),
            _ => {}
        }
    }
    bar(svg, 8.0, 74.0, 24.0, 4.0, p.muted_foreground);
    bar(svg, 73.0, 7.0, 48.0, 4.5, p.foreground);
    ring(svg, 215.0, 9.25, 3.0, p.muted_foreground.opacity(0.3), 1.3);
    let _ = write!(
        svg,
        r#"<circle cx="215" cy="9.25" r="3" fill="none" stroke="{}" stroke-width="1.3" stroke-dasharray="6.40885 18.84956" transform="rotate(-90 215 9.25)"/>"#,
        color(p.status_success)
    );
    rect(svg, 222.0, 7.0, 4.5, 4.5, 1.2, p.muted_foreground);
    rect(svg, 231.0, 7.0, 4.5, 4.5, 1.2, p.muted_foreground);
    for index in 0..7 {
        rect(
            svg,
            231.0,
            23.0 + index as f32 * 11.0,
            4.5,
            4.5,
            1.2,
            if index == 0 {
                p.foreground
            } else {
                p.muted_foreground
            },
        );
    }
    rect(
        svg,
        171.0,
        21.0,
        52.0,
        80.0,
        5.0,
        if dark {
            p.muted
        } else {
            elevation::mix(p.muted, p.background, 0.5)
        },
    );
    edge(svg, 171.25, 21.25, 51.5, 79.5, 4.75, edge_color);
    bar(svg, 177.0, 28.0, 18.0, 4.0, p.foreground);
    bar(svg, 177.0, 37.0, 40.0, 2.4, p.chat_divider);
    bar(svg, 177.0, 37.0, 13.6, 2.4, p.status_success);
    for (y, width, tone) in [(45.0, 18.0, p.pr_open), (54.0, 14.0, p.pr_merged)] {
        ring(svg, 179.5, y + 2.0, 2.2, tone, 1.1);
        bar(svg, 184.0, y, width, 4.0, tone);
        bar(svg, 187.0 + width, y, 30.0 - width, 4.0, p.muted_foreground);
    }
    bar(svg, 177.0, 66.0, 22.0, 4.0, p.foreground);
    for (y, width) in [(75.0, 22.0), (84.0, 28.0)] {
        ring(svg, 179.5, y + 2.0, 2.2, p.status_success, 1.1);
        bar(svg, 184.0, y, width, 4.0, p.muted_foreground);
    }
    rect(svg, 108.0, 18.0, 54.0, 14.0, 5.0, p.user_message_background);
    edge(
        svg,
        108.25,
        18.25,
        53.5,
        13.5,
        4.75,
        p.primary.opacity(0.05),
    );
    bar(svg, 74.0, 40.0, 20.0, 4.5, text);
    code(svg, 99.0, 40.0, 16.0, p);
    bar(svg, 121.0, 40.0, 41.0, 4.5, text);
    rect(
        svg,
        73.0,
        50.0,
        90.0,
        44.0,
        4.0,
        p.foreground.opacity(if dark { 0.03 } else { 0.04 }),
    );
    bar(svg, 77.0, 54.0, 30.0, 3.0, p.muted_foreground);
    bar(svg, 77.0, 60.0, 12.0, 4.0, text);
    let points = [
        (0.0, 0.75),
        (0.18, 0.65),
        (0.36, 0.8),
        (0.55, 0.62),
        (0.72, 0.3),
        (0.86, 0.1),
        (1.0, 0.0),
    ];
    let mut path = String::new();
    for (i, (x, y)) in points.iter().enumerate() {
        let _ = write!(
            path,
            "{}{:.1} {:.1} ",
            if i == 0 { "M" } else { "L" },
            77.0 + x * 82.0,
            69.0 + (y + 0.1) * 19.0 * 0.9
        );
    }
    let _ = write!(
        svg,
        r#"<defs><linearGradient id="{gradient}" x1="0" x2="0" y1="0" y2="1"><stop offset="0" stop-color="{accent}" stop-opacity="0.28"/><stop offset="1" stop-color="{accent}" stop-opacity="0"/></linearGradient></defs><path d="{path}L159 88 L77 88 Z" fill="url(#{gradient})"/><path d="{path}" fill="none" stroke="{accent}" stroke-width="1.3"/>"#,
        accent = color(p.primary)
    );
    dot(svg, 159.0, 70.71, 1.8, p.primary);
    dot(svg, 76.5, 104.25, 1.7, text);
    code(svg, 82.0, 102.0, 14.0, p);
    bar(svg, 100.0, 102.0, 62.0, 4.5, p.markdown_bold);
    rect(svg, 71.0, 116.0, 94.0, 28.0, 7.0, p.elevated);
    edge(svg, 71.25, 116.25, 93.5, 27.5, 6.75, edge_color);
    bar(svg, 78.0, 123.0, 52.8, 4.5, p.muted_foreground.opacity(0.6));
    ring(svg, 80.0, 136.25, 2.8, p.elevated_foreground, 1.3);
    bar(svg, 104.0, 134.0, 14.0, 4.5, p.elevated_foreground);
    bar(svg, 121.0, 134.0, 12.0, 4.5, p.warning.opacity(0.75));
    ring(svg, 140.0, 136.25, 2.6, p.status_success, 1.3);
    bar(svg, 145.0, 134.0, 9.0, 4.5, p.status_success);
    let _ = write!(
        svg,
        r#"<path d="M156 133 l6 3.5 l-6 3.5 z" fill="{}"/>"#,
        color(p.elevated_foreground.opacity(0.3))
    );
}
