//! Provider logos, as `packages/ui/src/hooks/useProviderLogo.ts` resolves
//! them and `ProviderLogo.tsx` draws them.
//!
//! A provider's logo is the first bundled logo among its candidates
//! (`buildProviderLogoCandidates` in `packages/ui/src/lib/providerLogoCandidates.ts`),
//! else models.dev's logo for its first candidate. The web draws logos with
//! `dark:invert`; [`invert_svg_colors`] gives the same drawing in the SVG's
//! own colors, since the native image pipeline has no color filter.

use std::collections::HashMap;

/// Where the web fetches a logo no bundled file covers.
const REMOTE_LOGO_BASE: &str = "https://models.dev/logos/";

/// Presentation attributes and style properties that carry a paint color.
const COLOR_PROPERTIES: [&str; 6] = [
    "fill",
    "stroke",
    "color",
    "stop-color",
    "flood-color",
    "lighting-color",
];

/// The logo files that may stand for `provider_id`, in lookup order.
///
/// `aliases` maps provider IDs onto logo files, as the web's
/// `PROVIDER_LOGO_ALIASES` does.
pub fn candidates(provider_id: &str, aliases: &HashMap<String, String>) -> Vec<String> {
    let normalized = normalize(provider_id);
    if normalized.is_empty() {
        return Vec::new();
    }
    let compact: String = normalized
        .chars()
        .filter(|&character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '-' | '.' | '/' | ':')
        })
        .collect();
    let primary = compact
        .split(['/', ':'])
        .next()
        .filter(|head| !head.is_empty())
        .unwrap_or(&compact);
    let prefix_alias = compact.starts_with("exe-").then_some("exe-dev");
    let ordered = [
        prefix_alias,
        aliases.get(compact.as_str()).map(String::as_str),
        aliases.get(primary).map(String::as_str),
        Some(compact.as_str()),
        Some(primary),
    ];
    let mut candidates: Vec<String> = Vec::new();
    for candidate in ordered.into_iter().flatten() {
        if !candidate.is_empty() && !candidates.iter().any(|known| known == candidate) {
            candidates.push(candidate.to_owned());
        }
    }
    candidates
}

/// Lowercased and trimmed, without a `models.` or `provider.` prefix, with
/// runs of whitespace as one `-`.
fn normalize(provider_id: &str) -> String {
    let lowered = provider_id.to_lowercase();
    let trimmed = lowered.trim();
    let unprefixed = trimmed.strip_prefix("models.").unwrap_or(trimmed);
    let unprefixed = unprefixed.strip_prefix("provider.").unwrap_or(unprefixed);
    let mut normalized = String::with_capacity(unprefixed.len());
    let mut in_whitespace = false;
    for character in unprefixed.chars() {
        if character.is_whitespace() {
            if !in_whitespace {
                normalized.push('-');
            }
            in_whitespace = true;
        } else {
            normalized.push(character);
            in_whitespace = false;
        }
    }
    normalized
}

/// The models.dev logo for a candidate.
pub fn remote_url(candidate: &str) -> String {
    format!("{REMOTE_LOGO_BASE}{candidate}.svg")
}

/// `svg` drawn as CSS `invert(1)` would draw it: every paint color in color
/// attributes and inline `style` declarations inverted, `currentColor` and
/// unpainted shapes (both black in an image) white.
///
/// Hex colors and the keywords `black`, `white` and `currentColor` are
/// inverted; `none`, references such as `url(#gradient)` and other color
/// syntaxes are kept, and `<style>` sheets are not read. None of the bundled
/// or models.dev logos uses those.
pub fn invert_svg_colors(svg: &str) -> String {
    let mut output = String::with_capacity(svg.len() + 16);
    let mut rest = svg;
    let mut root_seen = false;
    while let Some(open) = rest.find('<') {
        output.push_str(&rest[..open]);
        rest = &rest[open..];
        let Some(close) = tag_end(rest) else {
            break;
        };
        let tag = &rest[..=close];
        let is_root = !root_seen && is_svg_start(tag);
        root_seen |= is_root;
        output.push_str(&invert_tag(tag, is_root));
        rest = &rest[close + 1..];
    }
    output.push_str(rest);
    output
}

/// The index of the `>` that closes the tag `text` starts with, outside quotes.
fn tag_end(text: &str) -> Option<usize> {
    let mut quote = None;
    for (index, character) in text.char_indices() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), _) if character == open => quote = None,
            (None, '>') => return Some(index),
            _ => {}
        }
    }
    None
}

fn is_svg_start(tag: &str) -> bool {
    tag.strip_prefix("<svg")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|next| next.is_whitespace() || next == '>' || next == '/')
}

/// One start tag with its color attributes inverted. The root gets a white
/// fill when it sets none, as unpainted shapes would otherwise stay black.
fn invert_tag(tag: &str, is_root: bool) -> String {
    if tag.starts_with("</") || tag.starts_with("<!") || tag.starts_with("<?") {
        return tag.to_owned();
    }
    let mut output = String::with_capacity(tag.len() + 16);
    let mut rest = tag;
    let mut has_fill = false;
    while let Some(equals) = rest.find('=') {
        let name_start = rest[..equals]
            .rfind(|character: char| character.is_whitespace())
            .map_or(0, |index| index + 1);
        let name = rest[name_start..equals].trim();
        let after = &rest[equals + 1..];
        let Some(quote) = after
            .chars()
            .next()
            .filter(|&quote| quote == '"' || quote == '\'')
        else {
            output.push_str(&rest[..=equals]);
            rest = after;
            continue;
        };
        let Some(length) = after[1..].find(quote) else {
            break;
        };
        let value = &after[1..=length];
        output.push_str(&rest[..=equals]);
        output.push(quote);
        if name == "style" {
            output.push_str(&invert_declarations(value));
        } else if COLOR_PROPERTIES.contains(&name) {
            output.push_str(&invert_color(value));
        } else {
            output.push_str(value);
        }
        output.push(quote);
        has_fill |= name == "fill" || (name == "style" && declares_fill(value));
        rest = &after[length + 2..];
    }
    output.push_str(rest);
    if is_root && !has_fill {
        let insert_at = "<svg".len();
        output.insert_str(insert_at, " fill=\"#ffffff\"");
    }
    output
}

fn declares_fill(style: &str) -> bool {
    style
        .split(';')
        .filter_map(|declaration| declaration.split_once(':'))
        .any(|(property, _)| property.trim() == "fill")
}

/// `property: value` declarations with their color values inverted.
fn invert_declarations(style: &str) -> String {
    let declarations: Vec<String> = style
        .split(';')
        .map(|declaration| match declaration.split_once(':') {
            Some((property, value)) if COLOR_PROPERTIES.contains(&property.trim()) => {
                format!("{property}:{}", invert_color(value))
            }
            _ => declaration.to_owned(),
        })
        .collect();
    declarations.join(";")
}

/// One paint value inverted, keeping what is not a color it knows.
fn invert_color(value: &str) -> String {
    let trimmed = value.trim();
    let lowered = trimmed.to_ascii_lowercase();
    let rgb = match lowered.as_str() {
        "currentcolor" | "black" => Some([0, 0, 0]),
        "white" => Some([255, 255, 255]),
        _ => lowered.strip_prefix('#').and_then(parse_hex),
    };
    match rgb {
        Some([red, green, blue]) => {
            format!("#{:02x}{:02x}{:02x}", 255 - red, 255 - green, 255 - blue)
        }
        None => value.to_owned(),
    }
}

fn parse_hex(digits: &str) -> Option<[u8; 3]> {
    if !digits.chars().all(|digit| digit.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |text: &str| u8::from_str_radix(text, 16).ok();
    match digits.len() {
        3 => {
            let mut rgb = [0; 3];
            for (slot, digit) in rgb.iter_mut().zip(digits.chars()) {
                *slot = channel(&format!("{digit}{digit}"))?;
            }
            Some(rgb)
        }
        6 => Some([
            channel(&digits[0..2])?,
            channel(&digits[2..4])?,
            channel(&digits[4..6])?,
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web_aliases() -> HashMap<String, String> {
        [
            ("codex", "openai"),
            ("claude", "anthropic"),
            ("ollama-cloud", "ollama"),
            ("wafer-ai", "wafer.ai"),
            ("wafer", "wafer.ai"),
        ]
        .into_iter()
        .map(|(from, to)| (from.to_owned(), to.to_owned()))
        .collect()
    }

    /// Outputs of `buildProviderLogoCandidates` for the same inputs.
    #[test]
    fn candidates_follow_the_web_lookup() {
        let aliases = web_aliases();
        let cases: [(&str, &[&str]); 12] = [
            ("perf", &["perf"]),
            ("OpenAI", &["openai"]),
            ("models.anthropic", &["anthropic"]),
            ("provider.Claude", &["anthropic", "claude"]),
            ("codex/gpt", &["openai", "codex/gpt", "codex"]),
            ("exe-foo", &["exe-dev", "exe-foo"]),
            ("Ollama Cloud", &["ollama", "ollama-cloud"]),
            (
                "github-copilot:enterprise",
                &["github-copilot:enterprise", "github-copilot"],
            ),
            ("  Wafer AI ", &["wafer.ai", "wafer-ai"]),
            ("a!b@c", &["abc"]),
            ("", &[]),
            (":x", &[":x"]),
        ];
        for (provider_id, expected) in cases {
            assert_eq!(
                candidates(provider_id, &aliases),
                expected,
                "candidates of {provider_id:?}"
            );
        }
    }

    #[test]
    fn remote_logos_come_from_models_dev() {
        assert_eq!(remote_url("perf"), "https://models.dev/logos/perf.svg");
    }

    #[test]
    fn inversion_turns_black_paint_white_and_keeps_references() {
        let svg = r##"<svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg"><path fill="currentColor" d="M0 0"/><rect fill="url(#g)" stroke='#123' style="fill: white; opacity:0.5"/><circle fill="none"/></svg>"##;
        assert_eq!(
            invert_svg_colors(svg),
            r##"<svg fill="#ffffff" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg"><path fill="#ffffff" d="M0 0"/><rect fill="url(#g)" stroke='#eeddcc' style="fill:#000000; opacity:0.5"/><circle fill="none"/></svg>"##
        );
    }

    #[test]
    fn a_root_fill_is_inverted_rather_than_replaced() {
        assert_eq!(
            invert_svg_colors(r#"<svg fill="none"><path stroke="black" d="M0 0"/></svg>"#),
            r##"<svg fill="none"><path stroke="#ffffff" d="M0 0"/></svg>"##
        );
        assert_eq!(
            invert_svg_colors(r##"<?xml version="1.0"?><svg fill="#000000"></svg>"##),
            r##"<?xml version="1.0"?><svg fill="#ffffff"></svg>"##
        );
    }

    #[test]
    fn text_and_unknown_colors_are_kept() {
        assert_eq!(
            invert_svg_colors(r#"<svg fill="red"><title>a > b</title></svg>"#),
            r#"<svg fill="red"><title>a > b</title></svg>"#
        );
    }
}
