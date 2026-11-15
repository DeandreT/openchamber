//! Final CSS paint conversion. This is separate from the canonical import
//! arithmetic, which deliberately preserves CSS values it cannot measure.

mod spaces;

use spaces::{from_space, to_space};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaintError {
    Invalid,
    Unsupported(&'static str),
}

impl std::fmt::Display for PaintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => f.write_str("Invalid CSS color"),
            Self::Unsupported(reason) => write!(f, "Unsupported CSS color: {reason}"),
        }
    }
}

impl std::error::Error for PaintError {}

#[derive(Clone, Copy)]
pub(crate) struct Color {
    // Extended, encoded sRGB. Keep out-of-gamut channels until final painting.
    rgb: [f64; 3],
    alpha: f64,
    // Outside interpolation, a missing alpha paints as zero. During a mix it
    // takes the other endpoint's alpha, and remains missing if both omit it.
    missing_alpha: bool,
}

impl Color {
    fn rgba(self) -> Result<[f32; 4], PaintError> {
        if self
            .rgb
            .iter()
            .chain(std::iter::once(&self.alpha))
            .any(|v| !v.is_finite())
        {
            return Err(PaintError::Invalid);
        }
        Ok([
            self.rgb[0].clamp(0.0, 1.0) as f32,
            self.rgb[1].clamp(0.0, 1.0) as f32,
            self.rgb[2].clamp(0.0, 1.0) as f32,
            self.alpha.clamp(0.0, 1.0) as f32,
        ])
    }
}

/// Keep the inherited foreground's computed CSS metadata until final painting.
pub(crate) fn css_color_context(value: &str, dark: bool) -> Result<Color, PaintError> {
    let color = parse(value, dark, None, 0)?;
    color.rgba()?;
    Ok(color)
}

/// Convert a resolved CSS color to native sRGB channels in 0..=1.
/// `current_color` is the computed foreground inherited at the point of painting.
pub(crate) fn resolve_css_color(
    value: &str,
    dark: bool,
    current_color: Option<Color>,
) -> Result<[f32; 4], PaintError> {
    parse(value, dark, current_color, 0)?.rgba()
}

fn numeric(value: &str, percent_scale: f64) -> Result<f64, PaintError> {
    if value == "none" {
        return Err(PaintError::Unsupported("missing components"));
    }
    if value.contains('(') {
        return Err(PaintError::Unsupported("color math or relative colors"));
    }
    let (number, scale) = value
        .strip_suffix('%')
        .map_or((value, 1.0), |number| (number, percent_scale / 100.0));
    // CSS numbers require digits after a decimal point and after an exponent.
    // Rust's float parser also accepts forms such as `100.` and `1.e2`.
    let unsigned = number.strip_prefix(['+', '-']).unwrap_or(number);
    let (mantissa, exponent) = unsigned
        .split_once(['e', 'E'])
        .map_or((unsigned, None), |(mantissa, exponent)| {
            (mantissa, Some(exponent))
        });
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let valid_mantissa = match mantissa.split_once('.') {
        Some((integer, fraction)) => (integer.is_empty() || digits(integer)) && digits(fraction),
        None => digits(mantissa),
    };
    let valid_exponent = exponent
        .is_none_or(|exponent| digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)));
    if !valid_mantissa || !valid_exponent {
        return Err(PaintError::Invalid);
    }
    let result = number.parse::<f64>().map_err(|_| PaintError::Invalid)? * scale;
    if result.is_finite() {
        Ok(result)
    } else {
        Err(PaintError::Invalid)
    }
}

fn angle(value: &str) -> Result<f64, PaintError> {
    for (unit, factor) in [
        ("deg", 1.0),
        ("grad", 0.9),
        ("rad", 180.0 / std::f64::consts::PI),
        ("turn", 360.0),
    ] {
        if let Some(value) = value.strip_suffix(unit) {
            return Ok(numeric(value, 0.0)? * factor);
        }
    }
    if value.ends_with('%') {
        return Err(PaintError::Invalid);
    }
    numeric(value, 0.0)
}

fn split(body: &str, delimiter: char) -> Result<Vec<&str>, PaintError> {
    let (mut depth, mut start) = (0_u32, 0);
    let mut result = Vec::new();
    for (index, c) in body.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.checked_sub(1).ok_or(PaintError::Invalid)?,
            _ if c == delimiter && depth == 0 => {
                result.push(body[start..index].trim());
                start = index + c.len_utf8();
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(PaintError::Invalid);
    }
    result.push(body[start..].trim());
    Ok(result)
}

fn parse(
    value: &str,
    dark: bool,
    current: Option<Color>,
    depth: usize,
) -> Result<Color, PaintError> {
    if depth > 32 {
        return Err(PaintError::Unsupported("color nesting exceeds 32 levels"));
    }
    let lower = super::trim(value).to_ascii_lowercase();
    if lower == "transparent" {
        return Ok(Color {
            rgb: [0.0; 3],
            alpha: 0.0,
            missing_alpha: false,
        });
    }
    if lower == "currentcolor" {
        return current.ok_or(PaintError::Unsupported(
            "currentColor requires its inherited foreground",
        ));
    }
    let value = named_color(&lower).unwrap_or(&lower);
    if super::color::is_hex(value) {
        let short = &value[1..];
        let hex = if short.len() < 5 {
            short.chars().flat_map(|c| [c, c]).collect::<String>()
        } else {
            short.to_owned()
        };
        let channel = |at| {
            u8::from_str_radix(&hex[at..at + 2], 16)
                .map(|v| f64::from(v) / 255.0)
                .map_err(|_| PaintError::Invalid)
        };
        return Ok(Color {
            rgb: [channel(0)?, channel(2)?, channel(4)?],
            alpha: if hex.len() == 8 { channel(6)? } else { 1.0 },
            missing_alpha: false,
        });
    }
    let Some((function, body)) = value.split_once('(') else {
        return Err(PaintError::Unsupported(
            "system colors or CSS cascade keywords",
        ));
    };
    let body = body.strip_suffix(')').ok_or(PaintError::Invalid)?;
    if function == "color-mix" {
        return mixed(body, dark, current, depth + 1);
    }
    if function == "light-dark" {
        let pair = split(body, ',')?;
        if pair.len() != 2 {
            return Err(PaintError::Invalid);
        }
        // Both branches must be syntactically supported, as in CSS parsing.
        let light = parse(pair[0], dark, current, depth + 1)?;
        let dark_color = parse(pair[1], dark, current, depth + 1)?;
        return Ok(if dark { dark_color } else { light });
    }
    if body.trim_start().starts_with("from ") {
        return Err(PaintError::Unsupported("relative colors"));
    }
    if body.contains('(') {
        return Err(PaintError::Unsupported("color math"));
    }
    let legacy = body.contains(',');
    let mut missing_alpha = false;
    let (mut components, alpha) = if legacy {
        if !matches!(function, "rgb" | "rgba" | "hsl" | "hsla") || body.contains('/') {
            return Err(PaintError::Invalid);
        }
        let mut parts = split(body, ',')?;
        if parts.contains(&"none") {
            return Err(PaintError::Invalid);
        }
        let alpha = if parts.len() == 4 {
            numeric(parts.pop().expect("four components"), 1.0)?.clamp(0.0, 1.0)
        } else {
            1.0
        };
        (parts, alpha)
    } else {
        let pair = split(body, '/')?;
        if pair.len() > 2 {
            return Err(PaintError::Invalid);
        }
        let alpha = if pair.len() == 2 {
            missing_alpha = pair[1] == "none";
            if missing_alpha {
                0.0
            } else {
                numeric(pair[1], 1.0)?.clamp(0.0, 1.0)
            }
        } else {
            1.0
        };
        (pair[0].split_whitespace().collect::<Vec<_>>(), alpha)
    };
    let space = if function == "color" {
        if components.len() != 4 {
            return Err(PaintError::Invalid);
        }
        components.remove(0)
    } else {
        function
    };
    if function == "color"
        && !matches!(
            space,
            "srgb"
                | "srgb-linear"
                | "display-p3"
                | "a98-rgb"
                | "prophoto-rgb"
                | "rec2020"
                | "xyz"
                | "xyz-d50"
                | "xyz-d65"
        )
    {
        return Err(PaintError::Unsupported("color space"));
    }
    if components.len() != 3 {
        return Err(PaintError::Invalid);
    }
    let mut channels = [0.0; 3];
    match space {
        "rgb" | "rgba" => {
            if legacy && components.iter().filter(|c| c.ends_with('%')).count() % 3 != 0 {
                return Err(PaintError::Invalid);
            }
            for (channel, value) in channels.iter_mut().zip(&components) {
                *channel = numeric(value, 255.0)?.clamp(0.0, 255.0) / 255.0;
            }
            return Ok(Color {
                rgb: channels,
                missing_alpha,
                alpha,
            });
        }
        "hsl" | "hsla" | "hwb" => {
            channels[0] = angle(components[0])?;
            for index in 1..3 {
                if legacy && !components[index].ends_with('%') {
                    return Err(PaintError::Invalid);
                }
                let component = numeric(components[index], 100.0)? / 100.0;
                channels[index] = if space == "hwb" {
                    // HWB normalizes the original white:black ratio above 100%.
                    component.max(0.0)
                } else {
                    component.clamp(0.0, 1.0)
                };
            }
        }
        "lab" | "lch" | "oklab" | "oklch" => {
            let ok = space.starts_with("ok");
            channels[0] = numeric(components[0], if ok { 1.0 } else { 100.0 })?
                .clamp(0.0, if ok { 1.0 } else { 100.0 });
            channels[1] = numeric(
                components[1],
                if ok {
                    0.4
                } else if space == "lch" {
                    150.0
                } else {
                    125.0
                },
            )?;
            channels[2] = if space.ends_with("lch") {
                angle(components[2])?
            } else {
                numeric(components[2], if ok { 0.4 } else { 125.0 })?
            };
            if space.ends_with("lch") {
                channels[1] = channels[1].max(0.0);
            }
        }
        _ if function == "color" => {
            for (channel, value) in channels.iter_mut().zip(&components) {
                *channel = numeric(value, 1.0)?;
            }
        }
        _ => return Err(PaintError::Unsupported("color function")),
    }
    Ok(Color {
        rgb: from_space(if space == "hsla" { "hsl" } else { space }, channels)?,
        missing_alpha,
        alpha,
    })
}

fn weighted(
    value: &str,
    dark: bool,
    current: Option<Color>,
    depth: usize,
) -> Result<(Color, Option<f64>), PaintError> {
    let mut depth_count = 0;
    let mut first_boundary = None;
    let mut boundary = None;
    for (at, c) in value.char_indices() {
        if c == '(' {
            depth_count += 1;
        } else if c == ')' {
            depth_count -= 1;
        } else if c.is_whitespace() && depth_count == 0 {
            first_boundary.get_or_insert(at);
            boundary = Some(at);
        }
    }
    let leading = first_boundary.filter(|at| value[..*at].ends_with('%'));
    let trailing = boundary.filter(|at| value[*at..].trim().ends_with('%'));
    let (color, percentage) = match (leading, trailing) {
        (Some(_), Some(_)) => return Err(PaintError::Invalid),
        (Some(at), None) => (value[at..].trim(), Some(&value[..at])),
        (None, Some(at)) => (value[..at].trim(), Some(value[at..].trim())),
        (None, None) => (value, None),
    };
    let weight = if let Some(percentage) = percentage {
        let weight = numeric(percentage, 1.0)?;
        if !(0.0..=1.0).contains(&weight) {
            return Err(PaintError::Invalid);
        }
        Some(weight)
    } else {
        None
    };
    Ok((parse(color, dark, current, depth)?, weight))
}

fn mixed(
    body: &str,
    dark: bool,
    current: Option<Color>,
    depth: usize,
) -> Result<Color, PaintError> {
    let parts = split(body, ',')?;
    if parts.len() != 3 {
        return Err(PaintError::Invalid);
    }
    let mode: Vec<_> = parts[0].split_whitespace().collect();
    if mode.len() < 2 || mode[0] != "in" {
        return Err(PaintError::Invalid);
    }
    let space = mode[1];
    let hue_index = match space {
        "hsl" | "hwb" => Some(0),
        "lch" | "oklch" => Some(2),
        _ => None,
    };
    let hue_mode = if mode.len() == 2 {
        "shorter"
    } else if mode.len() == 4 && mode[3] == "hue" && hue_index.is_some() {
        mode[2]
    } else {
        return Err(PaintError::Invalid);
    };
    if !matches!(hue_mode, "shorter" | "longer" | "increasing" | "decreasing") {
        return Err(PaintError::Invalid);
    }
    let (mut a, wa) = weighted(parts[1], dark, current, depth)?;
    let (mut b, wb) = weighted(parts[2], dark, current, depth)?;
    let wa = wa.unwrap_or_else(|| wb.map_or(0.5, |weight| 1.0 - weight));
    let wb = wb.unwrap_or(1.0 - wa);
    let total = wa + wb;
    if total <= 0.0 {
        return Err(PaintError::Invalid);
    }
    let (wa, wb) = (wa / total, wb / total);
    let mut x = to_space(space, a.rgb)?;
    let mut y = to_space(space, b.rgb)?;
    let missing_alpha = a.missing_alpha && b.missing_alpha;
    if a.missing_alpha {
        a.alpha = b.alpha;
    }
    if b.missing_alpha {
        b.alpha = a.alpha;
    }
    if missing_alpha {
        // Keep the interpolated color for an outer mix that supplies alpha.
        a.alpha = 1.0;
        b.alpha = 1.0;
    }
    if let Some(index) = hue_index {
        // CSS makes hue powerless for achromatic colors, including transparent
        // endpoints in ordinary authored mixes. Carry the other endpoint hue.
        let powerless = |channels: [f64; 3]| match space {
            "hsl" => channels[1].abs() < 1e-7,
            "hwb" => channels[1] + channels[2] >= 1.0 - 1e-7,
            _ => channels[1].abs() < 1e-7,
        };
        if powerless(x) {
            x[index] = y[index];
        }
        if powerless(y) {
            y[index] = x[index];
        }
        x[index] = x[index].rem_euclid(360.0);
        y[index] = y[index].rem_euclid(360.0);
        let delta = y[index] - x[index];
        match hue_mode {
            "shorter" if delta > 180.0 => x[index] += 360.0,
            "shorter" if delta < -180.0 => y[index] += 360.0,
            "longer" if delta > 0.0 && delta < 180.0 => x[index] += 360.0,
            "longer" if delta <= 0.0 && delta > -180.0 => y[index] += 360.0,
            "increasing" if y[index] < x[index] => y[index] += 360.0,
            "decreasing" if x[index] < y[index] => x[index] += 360.0,
            _ => {}
        }
    }
    let alpha = a.alpha * wa + b.alpha * wb;
    let mut channels = [0.0; 3];
    for index in 0..3 {
        channels[index] = if hue_index == Some(index) {
            x[index] * wa + y[index] * wb
        } else if alpha == 0.0 {
            0.0
        } else {
            (x[index] * a.alpha * wa + y[index] * b.alpha * wb) / alpha
        };
    }
    Ok(Color {
        rgb: from_space(space, channels)?,
        missing_alpha,
        alpha: if missing_alpha {
            0.0
        } else {
            alpha * total.min(1.0)
        },
    })
}

fn named_color(name: &str) -> Option<&'static str> {
    // CSS Color named colors. Aliases retain their standardized RGB values.
    Some(match name {
        "aliceblue" => "#f0f8ff",
        "antiquewhite" => "#faebd7",
        "aqua" | "cyan" => "#00ffff",
        "aquamarine" => "#7fffd4",
        "azure" => "#f0ffff",
        "beige" => "#f5f5dc",
        "bisque" => "#ffe4c4",
        "black" => "#000000",
        "blanchedalmond" => "#ffebcd",
        "blue" => "#0000ff",
        "blueviolet" => "#8a2be2",
        "brown" => "#a52a2a",
        "burlywood" => "#deb887",
        "cadetblue" => "#5f9ea0",
        "chartreuse" => "#7fff00",
        "chocolate" => "#d2691e",
        "coral" => "#ff7f50",
        "cornflowerblue" => "#6495ed",
        "cornsilk" => "#fff8dc",
        "crimson" => "#dc143c",
        "darkblue" => "#00008b",
        "darkcyan" => "#008b8b",
        "darkgoldenrod" => "#b8860b",
        "darkgray" | "darkgrey" => "#a9a9a9",
        "darkgreen" => "#006400",
        "darkkhaki" => "#bdb76b",
        "darkmagenta" => "#8b008b",
        "darkolivegreen" => "#556b2f",
        "darkorange" => "#ff8c00",
        "darkorchid" => "#9932cc",
        "darkred" => "#8b0000",
        "darksalmon" => "#e9967a",
        "darkseagreen" => "#8fbc8f",
        "darkslateblue" => "#483d8b",
        "darkslategray" | "darkslategrey" => "#2f4f4f",
        "darkturquoise" => "#00ced1",
        "darkviolet" => "#9400d3",
        "deeppink" => "#ff1493",
        "deepskyblue" => "#00bfff",
        "dimgray" | "dimgrey" => "#696969",
        "dodgerblue" => "#1e90ff",
        "firebrick" => "#b22222",
        "floralwhite" => "#fffaf0",
        "forestgreen" => "#228b22",
        "fuchsia" | "magenta" => "#ff00ff",
        "gainsboro" => "#dcdcdc",
        "ghostwhite" => "#f8f8ff",
        "gold" => "#ffd700",
        "goldenrod" => "#daa520",
        "gray" | "grey" => "#808080",
        "green" => "#008000",
        "greenyellow" => "#adff2f",
        "honeydew" => "#f0fff0",
        "hotpink" => "#ff69b4",
        "indianred" => "#cd5c5c",
        "indigo" => "#4b0082",
        "ivory" => "#fffff0",
        "khaki" => "#f0e68c",
        "lavender" => "#e6e6fa",
        "lavenderblush" => "#fff0f5",
        "lawngreen" => "#7cfc00",
        "lemonchiffon" => "#fffacd",
        "lightblue" => "#add8e6",
        "lightcoral" => "#f08080",
        "lightcyan" => "#e0ffff",
        "lightgoldenrodyellow" => "#fafad2",
        "lightgray" | "lightgrey" => "#d3d3d3",
        "lightgreen" => "#90ee90",
        "lightpink" => "#ffb6c1",
        "lightsalmon" => "#ffa07a",
        "lightseagreen" => "#20b2aa",
        "lightskyblue" => "#87cefa",
        "lightslategray" | "lightslategrey" => "#778899",
        "lightsteelblue" => "#b0c4de",
        "lightyellow" => "#ffffe0",
        "lime" => "#00ff00",
        "limegreen" => "#32cd32",
        "linen" => "#faf0e6",
        "maroon" => "#800000",
        "mediumaquamarine" => "#66cdaa",
        "mediumblue" => "#0000cd",
        "mediumorchid" => "#ba55d3",
        "mediumpurple" => "#9370db",
        "mediumseagreen" => "#3cb371",
        "mediumslateblue" => "#7b68ee",
        "mediumspringgreen" => "#00fa9a",
        "mediumturquoise" => "#48d1cc",
        "mediumvioletred" => "#c71585",
        "midnightblue" => "#191970",
        "mintcream" => "#f5fffa",
        "mistyrose" => "#ffe4e1",
        "moccasin" => "#ffe4b5",
        "navajowhite" => "#ffdead",
        "navy" => "#000080",
        "oldlace" => "#fdf5e6",
        "olive" => "#808000",
        "olivedrab" => "#6b8e23",
        "orange" => "#ffa500",
        "orangered" => "#ff4500",
        "orchid" => "#da70d6",
        "palegoldenrod" => "#eee8aa",
        "palegreen" => "#98fb98",
        "paleturquoise" => "#afeeee",
        "palevioletred" => "#db7093",
        "papayawhip" => "#ffefd5",
        "peachpuff" => "#ffdab9",
        "peru" => "#cd853f",
        "pink" => "#ffc0cb",
        "plum" => "#dda0dd",
        "powderblue" => "#b0e0e6",
        "purple" => "#800080",
        "rebeccapurple" => "#663399",
        "red" => "#ff0000",
        "rosybrown" => "#bc8f8f",
        "royalblue" => "#4169e1",
        "saddlebrown" => "#8b4513",
        "salmon" => "#fa8072",
        "sandybrown" => "#f4a460",
        "seagreen" => "#2e8b57",
        "seashell" => "#fff5ee",
        "sienna" => "#a0522d",
        "silver" => "#c0c0c0",
        "skyblue" => "#87ceeb",
        "slateblue" => "#6a5acd",
        "slategray" | "slategrey" => "#708090",
        "snow" => "#fffafa",
        "springgreen" => "#00ff7f",
        "steelblue" => "#4682b4",
        "tan" => "#d2b48c",
        "teal" => "#008080",
        "thistle" => "#d8bfd8",
        "tomato" => "#ff6347",
        "turquoise" => "#40e0d0",
        "violet" => "#ee82ee",
        "wheat" => "#f5deb3",
        "white" => "#ffffff",
        "whitesmoke" => "#f5f5f5",
        "yellow" => "#ffff00",
        "yellowgreen" => "#9acd32",
        _ => return None,
    })
}

#[cfg(test)]
mod achromatic_tests {
    use super::resolve_css_color;

    #[test]
    fn lch_neutral_in_hsl_matches_retained_chrome_paint() {
        // Chrome 157 diagnostic-chrome-reference.json, retained under
        // theme-converter/missing-alpha, HTML SHA-256
        // 2acfa247c6bd50256d74bc2142ff2a258d25ace047300e1ad599413405df2ab3.
        // Chromium's rounded D50 conversions produce a small nonzero HSL
        // saturation here. Forcing hue carry from an exact gray changes paint.
        let value = "color-mix(in hsl, lch(60 0 40 / 0.7), #6699cc)";
        let samples = [[100.0_f32, 148.0, 154.0], [138.0, 186.0, 192.0]];
        for dark in [false, true] {
            let rgba = resolve_css_color(value, dark, None).expect("supported mix");
            for (background, expected) in samples.iter().enumerate() {
                for channel in 0..3 {
                    let painted =
                        (rgba[channel] * rgba[3] + background as f32 * (1.0 - rgba[3])) * 255.0;
                    assert!(
                        (painted.round() - expected[channel]).abs() <= 1.0,
                        "dark={dark}, background={background}, channel={channel}: native={painted}, Chrome={}",
                        expected[channel]
                    );
                }
            }
        }
    }
}
