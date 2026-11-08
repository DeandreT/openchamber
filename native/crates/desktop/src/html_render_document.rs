//! The document of a page an agent shows with `openchamber_render`.
//!
//! Ports `buildHtmlRenderDocument` and `htmlRenderTheme` from
//! `packages/ui/src/components/chat/message/parts/htmlRender.ts`. The
//! bootstrap's style and scripts and each built-in theme's variables come
//! from `resources/html-render.json`, which `bun run native:resources` cuts
//! out of the web's own document; this module only places them. The page
//! runs in a sandboxed frame without `allow-same-origin`, so its script has
//! an opaque origin and no reach into the desktop.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::Value;

const RESOURCE: &str = include_str!("../resources/html-render.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Resource {
    base_css: String,
    bootstrap_script: String,
    answer_script: String,
    metric_variables: Vec<String>,
    themes: Vec<ThemeVariables>,
}

#[derive(Deserialize)]
struct ThemeVariables {
    id: String,
    appearance: String,
    variables: Vec<(String, String)>,
    order: Vec<String>,
}

static BOOTSTRAP: LazyLock<Resource> =
    LazyLock::new(|| serde_json::from_str(RESOURCE).expect("html-render.json is generated valid"));

/// The app's sizes and fonts a page's variables follow (`HtmlRenderMetrics`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Metrics {
    /// The root font size in px; the interface size setting scales it.
    pub rem: f32,
    /// The chat's body text size in px.
    pub text: f32,
    pub sans: String,
    pub mono: String,
}

/// The variables a page styles against and the scheme it draws in.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PageTheme {
    pub dark: bool,
    pub variables: Vec<(String, String)>,
}

/// A number as JavaScript's template strings print it.
fn js_number(value: f32) -> String {
    let value = f64::from(value);
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

impl PageTheme {
    /// The variables of the built-in theme `id`, or `None` for a theme the
    /// export does not know.
    pub(crate) fn of(id: &str, metrics: &Metrics) -> Option<Self> {
        let theme = BOOTSTRAP.themes.iter().find(|theme| theme.id == id)?;
        let named: HashMap<&str, &str> = theme
            .variables
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let variables = theme
            .order
            .iter()
            .filter_map(|name| {
                let value = match name.as_str() {
                    "--oc-rem" => format!("{}px", js_number(metrics.rem)),
                    "--oc-text" => format!("{}px", js_number(metrics.text)),
                    "--radius" => format!("{}px", js_number(metrics.rem * 0.5625)),
                    "--font-sans" => metrics.sans.clone(),
                    "--font-mono" => metrics.mono.clone(),
                    _ => (*named.get(name.as_str())?).to_owned(),
                };
                debug_assert!(
                    BOOTSTRAP.metric_variables.contains(name) || named.contains_key(name.as_str()),
                    "every ordered variable is exported or a metric"
                );
                Some((name.clone(), value))
            })
            .collect();
        Some(Self {
            dark: theme.appearance == "dark",
            variables,
        })
    }

    fn appearance(&self) -> &'static str {
        if self.dark { "dark" } else { "light" }
    }

    /// The theme style's text (`themeCss`).
    pub(crate) fn css(&self) -> String {
        let mut css = format!(":root{{color-scheme:{};", self.appearance());
        for (name, value) in &self.variables {
            let valid = name.len() > 2
                && name.starts_with("--")
                && name[2..]
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
            if !valid {
                continue;
            }
            let value: String = value
                .chars()
                .filter(|character| !matches!(character, ';' | '{' | '}' | '<' | '>'))
                .collect();
            css.push_str(name);
            css.push(':');
            css.push_str(&value);
            css.push(';');
        }
        css.push('}');
        css.push_str(&BOOTSTRAP.base_css);
        css
    }

    /// The message that swaps a loaded page's theme (`htmlRenderThemeMessage`).
    pub(crate) fn message(&self) -> Value {
        let variables: serde_json::Map<String, Value> = self
            .variables
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect();
        serde_json::json!({
            "type": "openchamber:html-render:theme",
            "appearance": self.appearance(),
            "variables": variables,
        })
    }
}

/// Where a page stands on its answer when it loads (`HtmlRenderAnswerState`).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AnswerState {
    pub waiting: bool,
    pub answer: Option<Value>,
}

impl AnswerState {
    /// JSON inside a script element, with `<` escaped so no `</script>` in
    /// the data can close it.
    fn script_json(&self) -> String {
        let json = serde_json::json!({
            "waiting": self.waiting,
            "answer": self.answer.clone().unwrap_or(Value::Null),
        });
        json.to_string().replace('<', "\\u003c")
    }

    /// The message that tells a loaded page its state changed.
    pub(crate) fn message(&self) -> Value {
        serde_json::json!({
            "type": "openchamber:html-render:answer-state",
            "waiting": self.waiting,
            "answer": self.answer.clone().unwrap_or(Value::Null),
        })
    }
}

const RAW_TEXT_ELEMENTS: [&str; 6] = [
    "script", "style", "textarea", "title", "template", "noscript",
];

fn starts_with_ignore_case(haystack: &[u8], needle: &str) -> bool {
    haystack.len() >= needle.len()
        && haystack[..needle.len()].eq_ignore_ascii_case(needle.as_bytes())
}

fn is_js_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// The position just past the first `</name\s*>` at or after `from`.
fn closing_tag_end(bytes: &[u8], from: usize, name: &str) -> Option<usize> {
    let mut at = from;
    while at < bytes.len() {
        if bytes[at] == b'<' && bytes.get(at + 1) == Some(&b'/') {
            let after = at + 2;
            if starts_with_ignore_case(&bytes[after..], name) {
                let mut end = after + name.len();
                while end < bytes.len() && is_js_space(bytes[end]) {
                    end += 1;
                }
                if bytes.get(end) == Some(&b'>') {
                    return Some(end + 1);
                }
            }
        }
        at += 1;
    }
    None
}

/// Comments and raw-text elements blanked to the same length, so a `<head>`
/// written inside a script or comment is never where the bootstrap lands.
fn blank_non_markup(html: &str) -> Vec<u8> {
    let mut bytes = html.as_bytes().to_vec();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'<' {
            at += 1;
            continue;
        }
        let rest = &bytes[at..];
        let end = if rest.starts_with(b"<!--") {
            Some(
                rest[4..]
                    .windows(3)
                    .position(|window| window == b"-->")
                    .map_or(bytes.len(), |offset| at + 4 + offset + 3),
            )
        } else {
            RAW_TEXT_ELEMENTS.iter().find_map(|name| {
                let after = at + 1 + name.len();
                let named = starts_with_ignore_case(&rest[1..], name);
                let boundary = bytes
                    .get(after)
                    .is_none_or(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_'));
                (named && boundary)
                    .then(|| closing_tag_end(&bytes, after, name).unwrap_or(bytes.len()))
            })
        };
        match end {
            Some(end) => {
                bytes[at..end].fill(b' ');
                at = end;
            }
            None => at += 1,
        }
    }
    bytes
}

/// The end of the first `<name>` or `<name attributes>` tag.
fn open_tag_end(scan: &[u8], name: &str) -> Option<usize> {
    let mut at = 0;
    while at < scan.len() {
        if scan[at] == b'<' && starts_with_ignore_case(&scan[at + 1..], name) {
            let after = at + 1 + name.len();
            match scan.get(after) {
                Some(b'>') => return Some(after + 1),
                Some(byte) if is_js_space(*byte) => {
                    if let Some(close) = scan[after..].iter().position(|byte| *byte == b'>') {
                        return Some(after + close + 1);
                    }
                }
                _ => {}
            }
        }
        at += 1;
    }
    None
}

/// Whether some `<meta` tag in `scan` names `attribute` before its end.
fn has_meta(scan: &[u8], attribute: impl Fn(&[u8]) -> bool) -> bool {
    let mut at = 0;
    while let Some(offset) = scan[at..]
        .windows(5)
        .position(|window| window.eq_ignore_ascii_case(b"<meta"))
    {
        let start = at + offset + 5;
        if scan.get(start).is_some_and(|byte| is_js_space(*byte)) {
            let end = scan[start..]
                .iter()
                .position(|byte| *byte == b'>')
                .map_or(scan.len(), |close| start + close);
            if attribute(&scan[start..end]) {
                return true;
            }
        }
        at = start;
    }
    false
}

fn names_charset(tag: &[u8]) -> bool {
    tag.windows(7)
        .any(|window| window.eq_ignore_ascii_case(b"charset"))
}

/// `name\s*=\s*["']?viewport` inside one tag.
fn names_viewport(tag: &[u8]) -> bool {
    let mut at = 0;
    while let Some(offset) = tag[at..]
        .windows(4)
        .position(|window| window.eq_ignore_ascii_case(b"name"))
    {
        let mut position = at + offset + 4;
        while tag.get(position).is_some_and(|byte| is_js_space(*byte)) {
            position += 1;
        }
        if tag.get(position) == Some(&b'=') {
            position += 1;
            while tag.get(position).is_some_and(|byte| is_js_space(*byte)) {
                position += 1;
            }
            if matches!(tag.get(position), Some(b'"' | b'\'')) {
                position += 1;
            }
            if starts_with_ignore_case(&tag[position.min(tag.len())..], "viewport") {
                return true;
            }
        }
        at += offset + 4;
    }
    false
}

/// The leading `\s*<!doctype ...>`, if the page starts with one.
fn doctype_end(scan: &[u8]) -> Option<usize> {
    let start = scan.iter().position(|byte| !is_js_space(*byte))?;
    if !starts_with_ignore_case(&scan[start..], "<!doctype") {
        return None;
    }
    scan[start..]
        .iter()
        .position(|byte| *byte == b'>')
        .map(|close| start + close + 1)
}

/// The agent's page with the bootstrap first in its head. `contained` is for
/// a frame with nothing around it to scroll: the page's scrolling stops at
/// its edges without the elastic bounce.
pub(crate) fn build_document(
    html: &str,
    theme: &PageTheme,
    contained: bool,
    answer: &AnswerState,
) -> String {
    let scan = blank_non_markup(html);
    let charset_window = &scan[..scan.len().min(4096)];
    let mut bootstrap = String::new();
    if !has_meta(charset_window, names_charset) {
        bootstrap.push_str("<meta charset=\"utf-8\">");
    }
    if !has_meta(&scan, names_viewport) {
        bootstrap
            .push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    }
    bootstrap.push_str("<style id=\"oc-html-render-theme\">");
    bootstrap.push_str(&theme.css());
    bootstrap.push_str("</style>");
    if contained {
        bootstrap.push_str("<style>html{overscroll-behavior:none}</style>");
    }
    bootstrap.push_str("<script>");
    bootstrap.push_str(&BOOTSTRAP.bootstrap_script);
    bootstrap.push_str("</script><script type=\"application/json\" id=\"oc-html-render-answer\">");
    bootstrap.push_str(&answer.script_json());
    bootstrap.push_str("</script><script>");
    bootstrap.push_str(&BOOTSTRAP.answer_script);
    bootstrap.push_str("</script>");
    if let Some(at) = open_tag_end(&scan, "head") {
        return format!("{}{bootstrap}{}", &html[..at], &html[at..]);
    }
    if let Some(at) = open_tag_end(&scan, "html").or_else(|| doctype_end(&scan)) {
        return format!("{}<head>{bootstrap}</head>{}", &html[..at], &html[at..]);
    }
    format!("<!doctype html><head>{bootstrap}</head>{html}")
}

/// A page's reported height, kept inside the frame's bounds
/// (`clampHtmlRenderHeight`).
pub(crate) fn clamp_height(height: f64) -> f32 {
    const MIN: f64 = 48.0;
    const MAX: f64 = 20_000.0;
    if !height.is_finite() {
        return MIN as f32;
    }
    height.ceil().clamp(MIN, MAX) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Samples {
        sample_theme: String,
        sample_metrics: SampleMetrics,
        samples: Vec<Sample>,
    }

    #[derive(Deserialize)]
    struct SampleMetrics {
        rem: f32,
        text: f32,
        sans: String,
        mono: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Sample {
        html: String,
        contained: bool,
        answer_state: SampleAnswer,
        document: String,
    }

    #[derive(Deserialize)]
    struct SampleAnswer {
        waiting: bool,
        answer: Option<Value>,
    }

    #[test]
    fn documents_match_the_web_builder() {
        let samples: Samples = serde_json::from_str(RESOURCE).expect("samples");
        let metrics = Metrics {
            rem: samples.sample_metrics.rem,
            text: samples.sample_metrics.text,
            sans: samples.sample_metrics.sans,
            mono: samples.sample_metrics.mono,
        };
        let theme = PageTheme::of(&samples.sample_theme, &metrics).expect("theme");
        for sample in samples.samples {
            let answer = AnswerState {
                waiting: sample.answer_state.waiting,
                answer: sample.answer_state.answer,
            };
            assert_eq!(
                build_document(&sample.html, &theme, sample.contained, &answer),
                sample.document,
                "{}",
                sample.html
            );
        }
    }

    #[test]
    fn every_theme_has_variables() {
        let metrics = Metrics {
            rem: 15.0,
            text: 13.125,
            sans: "S".to_owned(),
            mono: "M".to_owned(),
        };
        for theme in &BOOTSTRAP.themes {
            let page = PageTheme::of(&theme.id, &metrics).expect("known theme");
            assert!(page.css().contains("--radius:8.4375px;"), "{}", theme.id);
        }
        assert!(PageTheme::of("missing-theme", &metrics).is_none());
    }

    #[test]
    fn answer_data_cannot_close_its_script() {
        let answer = AnswerState {
            waiting: false,
            answer: Some(serde_json::json!({ "x": "</script><img src=x>" })),
        };
        assert!(!answer.script_json().contains("</script>"));
    }

    #[test]
    fn heights_stay_inside_the_frame_bounds() {
        assert_eq!(clamp_height(10.0), 48.0);
        assert_eq!(clamp_height(120.2), 121.0);
        assert_eq!(clamp_height(f64::NAN), 48.0);
        assert_eq!(clamp_height(1e9), 20_000.0);
    }
}
