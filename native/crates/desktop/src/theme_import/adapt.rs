//! Shared VS Code role adaptation, in the order of `vscode/adapt.ts`.

use super::color::{contrast, hue, mix, on, opacity, readable, tint};
use super::{Colors, fallback, get};

// Evaluate a derived color before borrowing its destination mutably. Several
// canonical assignments read another role from the same palette.
macro_rules! set {
    ($colors:expr, $key:expr, $value:expr $(,)?) => {{
        let value = $value;
        super::set($colors, $key, value);
    }};
}

fn ratio(color: &str, background: &str, canvas: &str) -> f64 {
    contrast(color, background, canvas).unwrap_or(1.0)
}

fn toward(seed: &str, target: &str, canvas: &str, accepts: impl Fn(&str) -> bool) -> String {
    if accepts(seed) {
        return seed.to_owned();
    }
    let (mut low, mut high) = (0.0, 1.0);
    let mut result = target.to_owned();
    for _ in 0..16 {
        let amount = (low + high) / 2.0;
        let candidate = mix(target, seed, amount, canvas);
        if accepts(&candidate) {
            high = amount;
            result = candidate;
        } else {
            low = amount;
        }
    }
    result
}

fn minimum(color: &str, surfaces: &[String], canvas: &str) -> f64 {
    surfaces
        .iter()
        .map(|surface| ratio(color, surface, canvas))
        .fold(f64::INFINITY, f64::min)
}

fn readable_on(color: &str, neutral: &str, surfaces: &[String], canvas: &str) -> String {
    toward(color, neutral, canvas, |c| {
        minimum(c, surfaces, canvas) >= 4.6
    })
}

fn text(
    seed: &str,
    ceiling: f64,
    floor: f64,
    neutral: &str,
    surfaces: &[String],
    canvas: &str,
) -> String {
    let seed = toward(seed, canvas, canvas, |c| {
        ratio(c, canvas, canvas) <= ceiling
    });
    toward(&seed, neutral, canvas, |c| {
        minimum(c, surfaces, canvas) >= floor
    })
}

fn border(seed: &str, caps: [f64; 3], surfaces: &[String], canvas: &str) -> String {
    let strongest = caps.into_iter().fold(f64::NEG_INFINITY, f64::max);
    let quiet = |color: &str| {
        caps.iter()
            .zip(surfaces)
            .all(|(cap, surface)| ratio(color, surface, canvas) <= cap + (strongest - cap) * 0.2)
    };
    if quiet(seed) {
        return seed.to_owned();
    }
    let (mut low, mut high) = (0.0, 1.0);
    let mut result = opacity(seed, 0.0);
    for _ in 0..16 {
        let alpha = (low + high) / 2.0;
        let candidate = opacity(seed, alpha);
        if quiet(&candidate) {
            low = alpha;
            result = candidate;
        } else {
            high = alpha;
        }
    }
    result
}

fn semantic(
    seed: &str,
    fallback: &str,
    range: [f64; 2],
    neutral: &str,
    surfaces: &[String],
    canvas: &str,
) -> String {
    let valid = hue(seed, canvas).is_some_and(|hue| hue >= range[0] && hue <= range[1]);
    readable_on(
        if valid { seed } else { fallback },
        neutral,
        surfaces,
        canvas,
    )
}

pub(super) fn apply(c: &mut Colors, authored: &Colors, dark: bool, high_contrast: bool) {
    if high_contrast {
        return;
    }
    let canvas = get(c, "surface.background");
    let neutral = on(&canvas, &canvas);
    let floor = 4.6_f64.min(ratio(&neutral, &canvas, &canvas));
    let layer = |seed: &str, lighter: bool| {
        let target = if lighter { "#ffffff" } else { "#000000" };
        let seed_luminance = ratio(seed, "#000000", &canvas);
        let canvas_luminance = ratio(&canvas, "#000000", &canvas);
        let correct = if lighter {
            seed_luminance > canvas_luminance
        } else {
            seed_luminance < canvas_luminance
        };
        let start = if correct {
            mix(seed, &canvas, 1.0, &canvas)
        } else {
            canvas.clone()
        };
        let quiet = toward(&start, &canvas, &canvas, |color| {
            ratio(color, &canvas, &canvas) <= 1.10
        });
        let separated = if ratio(target, &canvas, &canvas) < 1.04 {
            target.to_owned()
        } else {
            toward(&quiet, target, &canvas, |color| {
                ratio(color, &canvas, &canvas) >= 1.04
            })
        };
        toward(&separated, &canvas, &canvas, |color| {
            ratio(&neutral, color, &canvas) >= floor
        })
    };
    let muted = layer(&get(c, "surface.muted"), false);
    let elevated = layer(&get(c, "surface.elevated"), dark);
    set!(c, "surface.muted", &muted);
    set!(c, "surface.elevated", &elevated);
    set!(c, "surface.subtle", mix(&neutral, &canvas, 0.025, &canvas));
    let mut surfaces = vec![canvas.clone(), muted, elevated];
    for key in ["surface.foreground", "surface.elevatedForeground"] {
        set!(
            c,
            key,
            text(
                &get(c, key),
                if dark { 13.6 } else { 9.0 },
                7.0,
                &neutral,
                &surfaces,
                &canvas,
            ),
        );
        if key == "surface.foreground" {
            let ceiling = (if dark { 5.2_f64 } else { 6.0_f64 })
                .min(ratio(&get(c, key), &canvas, &canvas) * 0.7);
            set!(
                c,
                "surface.mutedForeground",
                text(
                    &get(c, "surface.mutedForeground"),
                    ceiling,
                    4.6,
                    &neutral,
                    &surfaces,
                    &canvas,
                ),
            );
        }
    }
    let accent = [
        "button.background",
        "textLink.foreground",
        "activityBarBadge.background",
        "list.highlightForeground",
        "focusBorder",
    ]
    .iter()
    .filter_map(|key| authored.get(*key))
    .find(|color| minimum(color, &surfaces, &canvas) >= 3.0)
    .cloned()
    .unwrap_or_else(|| get(c, "primary.base"));
    let primary = readable_on(&accent, &neutral, &surfaces, &canvas);
    set!(c, "primary.base", &primary);
    set!(c, "primary.foreground", on(&primary, &canvas));
    set!(c, "primary.hover", mix(&neutral, &primary, 0.08, &canvas));
    set!(c, "primary.active", mix(&neutral, &primary, 0.16, &canvas));
    set!(c, "primary.muted", opacity(&primary, 0.5));
    if minimum(&get(c, "interactive.focusRing"), &surfaces, &canvas) < 1.1 {
        for key in ["focus", "focusRing", "borderFocus"] {
            set!(c, &format!("interactive.{key}"), &primary);
        }
    }
    let selection = toward(
        &get(c, "interactive.selection"),
        &mix(&primary, &canvas, 0.18, &canvas),
        &canvas,
        |color| minimum(color, &surfaces, &canvas) >= 1.1,
    );
    set!(c, "interactive.selection", selection);
    let (border_caps, hover_caps, tools_caps, divider_caps) = if dark {
        (
            [1.217, 1.245, 1.117],
            [2.304, 2.356, 2.115],
            [1.199, 1.207, 1.156],
            [1.410, 1.442, 1.294],
        )
    } else {
        (
            [1.268, 1.203, 1.162],
            [1.640, 1.557, 1.503],
            [1.231, 1.195, 1.171],
            [1.427, 1.355, 1.308],
        )
    };
    for (key, caps) in [
        ("border", border_caps),
        ("borderHover", hover_caps),
        ("hover", [1.18; 3]),
        ("active", [1.25; 3]),
        ("selection", [1.6, 1.7, 1.45]),
    ] {
        let path = format!("interactive.{key}");
        set!(c, &path, border(&get(c, &path), caps, &surfaces, &canvas));
    }
    set!(
        c,
        "interactive.selectionForeground",
        readable(
            &get(c, "surface.foreground"),
            &get(c, "interactive.selection"),
            &canvas,
        ),
    );
    for (role, fallback, range) in [
        (
            "error",
            if dark { "#e07777" } else { "#b43b45" },
            [0.0, 35.0],
        ),
        (
            "warning",
            if dark { "#d6b467" } else { "#916b17" },
            [35.0, 100.0],
        ),
        (
            "success",
            if dark { "#80b888" } else { "#357b47" },
            [105.0, 170.0],
        ),
        (
            "info",
            if dark { "#83afe0" } else { "#366eaa" },
            [185.0, 275.0],
        ),
    ] {
        let key = format!("status.{role}");
        let color = semantic(&get(c, &key), fallback, range, &neutral, &surfaces, &canvas);
        set!(c, &key, &color);
        set!(c, &format!("{key}Foreground"), on(&color, &canvas));
        set!(
            c,
            &format!("{key}Background"),
            opacity(&color, if dark { 0.16 } else { 0.12 }),
        );
        set!(
            c,
            &format!("{key}Border"),
            opacity(&color, if dark { 0.45 } else { 0.35 }),
        );
    }
    set!(c, "pr.open", get(c, "status.success"));
    set!(c, "pr.closed", get(c, "status.error"));
    set!(
        c,
        "pr.blocked",
        semantic(
            &get(authored, "terminal.ansiYellow"),
            if dark { "#d99464" } else { "#a65b25" },
            [40.0, 65.0],
            &neutral,
            &surfaces,
            &canvas,
        ),
    );
    set!(
        c,
        "pr.merged",
        semantic(
            &get(authored, "terminal.ansiMagenta"),
            if dark { "#b79bd9" } else { "#8255b0" },
            [285.0, 325.0],
            &neutral,
            &surfaces,
            &canvas,
        ),
    );

    let mut base = toward(&canvas, &neutral, &canvas, |color| {
        ratio(color, &canvas, &canvas) >= 1.12
    });
    if ratio(&neutral, &base, &canvas) < floor {
        base = toward(
            &canvas,
            if neutral == "#ffffff" {
                "#000000"
            } else {
                "#ffffff"
            },
            &canvas,
            |color| ratio(color, &canvas, &canvas) >= 1.12,
        );
    }
    let bubble = toward(
        &tint(&primary, &base, if dark { 0.16 } else { 0.06 }, &canvas),
        &base,
        &canvas,
        |color| ratio(color, &canvas, &canvas) >= 1.1 && ratio(&neutral, color, &canvas) >= floor,
    );
    set!(c, "chat.userMessageBackground", &bubble);
    // Border caps always use the original three surfaces; text also covers the bubble.
    let border_surfaces = surfaces.clone();
    surfaces.push(bubble.clone());
    set!(
        c,
        "surface.foreground",
        toward(&get(c, "surface.foreground"), &neutral, &canvas, |color| {
            minimum(color, &surfaces, &canvas) >= 7.0
        }),
    );
    set!(
        c,
        "surface.mutedForeground",
        readable_on(
            &get(c, "surface.mutedForeground"),
            &neutral,
            &surfaces,
            &canvas,
        ),
    );
    let foreground = get(c, "surface.foreground");
    let muted_text = get(c, "surface.mutedForeground");
    // The definition boundary retains only background, bubble and divider.
    // Other live-adapter chat roles never reach a saved file or its resolver.
    set!(
        c,
        "chat.divider",
        border(
            &get(c, "chat.divider"),
            divider_caps,
            &border_surfaces,
            &canvas,
        ),
    );
    set!(c, "pr.draft", &muted_text);
    set!(
        c,
        "tools.border",
        border(
            &get(c, "tools.border"),
            tools_caps,
            &border_surfaces,
            &canvas,
        ),
    );
    set!(c, "tools.title", &foreground);
    set!(c, "tools.description", &muted_text);
    set!(c, "tools.icon", &muted_text);
    set!(
        c,
        "markdown.bold",
        mix(&neutral, &foreground, 0.12, &canvas),
    );
    set!(c, "markdown.italic", &foreground);
    set!(c, "markdown.blockquote", &muted_text);
    for key in ["blockquoteBorder", "hr"] {
        set!(
            c,
            &format!("markdown.{key}"),
            border(
                &fallback(
                    c,
                    &[&format!("markdown.{key}")],
                    &get(c, "interactive.border"),
                ),
                divider_caps,
                &border_surfaces,
                &canvas,
            ),
        );
    }
    for (key, token, value) in [
        ("link", "textLink.foreground", primary.clone()),
        (
            "linkHover",
            "textLink.activeForeground",
            get(c, "primary.hover"),
        ),
    ] {
        set!(
            c,
            &format!("markdown.{key}"),
            readable_on(
                &fallback(authored, &[token], &value),
                &neutral,
                &surfaces,
                &canvas,
            ),
        );
    }
    set!(c, "markdown.listMarker", opacity(&primary, 0.6));
    set!(c, "markdown.inlineCodeBackground", get(c, "surface.subtle"));
    set!(
        c,
        "markdown.inlineCode",
        readable(
            &readable_on(
                &fallback(authored, &["textPreformat.foreground"], &primary),
                &neutral,
                &surfaces,
                &canvas,
            ),
            &get(c, "surface.subtle"),
            &canvas,
        ),
    );
    let code_background = toward(
        &tint(&canvas, &get(c, "syntax.base.background"), 1.0, &canvas),
        &canvas,
        &canvas,
        |color| ratio(color, &canvas, &canvas) <= 1.1,
    );
    set!(c, "syntax.base.background", &code_background);
    set!(
        c,
        "syntax.base.foreground",
        readable(&foreground, &code_background, &canvas),
    );
}
