//! `packages/ui/src/lib/theme/color.ts`, including its legacy CSS fallbacks.

#[derive(Clone, Copy)]
struct Color {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}

pub(super) fn is_hex(value: &str) -> bool {
    value.strip_prefix('#').is_some_and(|hex| {
        matches!(hex.len(), 3 | 4 | 6 | 8) && hex.bytes().all(|c| c.is_ascii_hexdigit())
    })
}

fn parse(value: &str) -> Option<Color> {
    let value = super::trim(value);
    if is_hex(value) {
        let hex = &value[1..];
        let full = if hex.len() < 5 {
            hex.chars().flat_map(|c| [c, c]).collect::<String>()
        } else {
            hex.to_owned()
        };
        let channel = |offset| {
            u8::from_str_radix(&full[offset..offset + 2], 16)
                .ok()
                .map(f64::from)
        };
        return Some(Color {
            r: channel(0)?,
            g: channel(2)?,
            b: channel(4)?,
            a: if full.len() == 8 {
                channel(6)? / 255.0
            } else {
                1.0
            },
        });
    }
    let lowercase: String = value
        .chars()
        .map(|c| {
            if super::whitespace(c) {
                ' '
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    let body = lowercase
        .strip_prefix("rgba(")
        .or_else(|| lowercase.strip_prefix("rgb("))?
        .strip_suffix(')')?;
    // Match the deliberately small numeric rgb grammar in color.ts. Named,
    // percent, HSL and other CSS colors pass through the arithmetic unchanged.
    let bytes = body.as_bytes();
    let mut at = 0;
    let whitespace = |at: &mut usize| {
        while *at < bytes.len() && bytes[*at].is_ascii_whitespace() {
            *at += 1;
        }
    };
    let number = |at: &mut usize| -> Option<f64> {
        let start = *at;
        while *at < bytes.len() && (bytes[*at].is_ascii_digit() || bytes[*at] == b'.') {
            *at += 1;
        }
        body[start..*at].parse().ok()
    };
    whitespace(&mut at);
    let mut values = [0.0; 3];
    for (index, slot) in values.iter_mut().enumerate() {
        *slot = number(&mut at)?;
        if !(0.0..=255.0).contains(slot) {
            return None;
        }
        if index < 2 {
            let start = at;
            while at < bytes.len() && (bytes[at] == b',' || bytes[at].is_ascii_whitespace()) {
                at += 1;
            }
            if at == start {
                return None;
            }
        }
    }
    whitespace(&mut at);
    let a = if at < bytes.len() && matches!(bytes[at], b',' | b'/') {
        at += 1;
        whitespace(&mut at);
        let a = number(&mut at)?;
        whitespace(&mut at);
        a
    } else {
        1.0
    };
    if at != bytes.len() || !(0.0..=1.0).contains(&a) {
        return None;
    }
    Some(Color {
        r: values[0],
        g: values[1],
        b: values[2],
        a,
    })
}

fn over(front: Color, back: Color) -> Color {
    Color {
        r: front.r * front.a + back.r * (1.0 - front.a),
        g: front.g * front.a + back.g * (1.0 - front.a),
        b: front.b * front.a + back.b * (1.0 - front.a),
        a: 1.0,
    }
}

fn luminance(color: Color) -> f64 {
    let linear = |v: f64| {
        if v / 255.0 <= 0.04045 {
            v / 255.0 / 12.92
        } else {
            ((v / 255.0 + 0.055) / 1.055).powf(2.4)
        }
    };
    linear(color.r) * 0.2126 + linear(color.g) * 0.7152 + linear(color.b) * 0.0722
}

fn ratio(a: Color, b: Color) -> f64 {
    let x = luminance(a);
    let y = luminance(b);
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

fn hex(color: Color) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        color.r.round() as u32,
        color.g.round() as u32,
        color.b.round() as u32
    )
}

pub(super) fn contrast(foreground: &str, background: &str, canvas: &str) -> Option<f64> {
    let surface = over(parse(background)?, parse(canvas)?);
    Some(ratio(over(parse(foreground)?, surface), surface))
}

pub(super) fn readable(foreground: &str, background: &str, canvas: &str) -> String {
    let (Some(fg), Some(bg), Some(under)) = (parse(foreground), parse(background), parse(canvas))
    else {
        return foreground.to_owned();
    };
    let surface = over(bg, under);
    if ratio(over(fg, surface), surface) >= 4.5 {
        return foreground.to_owned();
    }
    let black = Color {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
    let white = Color {
        r: 255.0,
        g: 255.0,
        b: 255.0,
        a: 1.0,
    };
    let target = if ratio(black, surface) >= ratio(white, surface) {
        black
    } else {
        white
    };
    let (mut low, mut high, mut result) = (0.0, 1.0, target);
    for _ in 0..16 {
        let amount = (low + high) / 2.0;
        let candidate = Color {
            r: (fg.r + (target.r - fg.r) * amount).round(),
            g: (fg.g + (target.g - fg.g) * amount).round(),
            b: (fg.b + (target.b - fg.b) * amount).round(),
            a: 1.0,
        };
        if ratio(candidate, surface) >= 4.6 {
            result = candidate;
            high = amount;
        } else {
            low = amount;
        }
    }
    hex(result)
}

pub(super) fn on(background: &str, canvas: &str) -> String {
    match (
        contrast("#000000", background, canvas),
        contrast("#ffffff", background, canvas),
    ) {
        (Some(black), Some(white)) if black >= white => "#000000",
        _ => "#ffffff",
    }
    .to_owned()
}

pub(super) fn opacity(value: &str, alpha: f64) -> String {
    match parse(value) {
        Some(color) => format!("{}{:02x}", hex(color), (alpha * 255.0).round() as u32),
        None => format!(
            "color-mix(in srgb, {value} {}%, transparent)",
            alpha * 100.0
        ),
    }
}

pub(super) fn mix(foreground: &str, background: &str, amount: f64, canvas: &str) -> String {
    match (parse(foreground), parse(background), parse(canvas)) {
        (Some(mut fg), Some(bg), Some(under)) => {
            fg.a *= amount;
            hex(over(fg, over(bg, under)))
        }
        _ => format!(
            "color-mix(in srgb, {foreground} {}%, {background})",
            amount * 100.0
        ),
    }
}

fn oklab(color: Color) -> [f64; 3] {
    let linear = |v: f64| {
        if v / 255.0 <= 0.04045 {
            v / 3294.6
        } else {
            ((v / 255.0 + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (linear(color.r), linear(color.g), linear(color.b));
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    [
        0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
    ]
}

fn from_oklab(lab: [f64; 3]) -> Color {
    let l = (lab[0] + 0.3963377774 * lab[1] + 0.2158037573 * lab[2]).powi(3);
    let m = (lab[0] - 0.1055613458 * lab[1] - 0.0638541728 * lab[2]).powi(3);
    let s = (lab[0] - 0.0894841775 * lab[1] - 1.291485548 * lab[2]).powi(3);
    let gamma = |v: f64| {
        255.0
            * (if v <= 0.0031308 {
                v * 12.92
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            })
            .clamp(0.0, 1.0)
    };
    Color {
        r: gamma(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
        g: gamma(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
        b: gamma(-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s),
        a: 1.0,
    }
}

pub(super) fn tint(accent: &str, surface: &str, amount: f64, canvas: &str) -> String {
    let (Some(tint), Some(base), Some(under)) = (parse(accent), parse(surface), parse(canvas))
    else {
        return surface.to_owned();
    };
    let x = oklab(over(base, under));
    let y = oklab(over(tint, under));
    hex(from_oklab([
        x[0],
        x[1] + (y[1] - x[1]) * amount,
        x[2] + (y[2] - x[2]) * amount,
    ]))
}

pub(super) fn distance(first: &str, second: &str, background: &str, canvas: &str) -> Option<f64> {
    let surface = over(parse(background)?, parse(canvas)?);
    let x = oklab(over(parse(first)?, surface));
    let y = oklab(over(parse(second)?, surface));
    Some((x[1] - y[1]).hypot(x[2] - y[2]))
}

pub(super) fn hue(value: &str, canvas: &str) -> Option<f64> {
    let lab = oklab(over(parse(value)?, parse(canvas)?));
    (lab[1].hypot(lab[2]) >= 0.025).then(|| (lab[2].atan2(lab[1]).to_degrees() + 360.0) % 360.0)
}
