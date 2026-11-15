//! Pure ports of the shared UI's theme import and definition boundaries.
//!
//! This module preserves authored CSS strings. Painting those strings belongs
//! to the native theme consumer; resolving a theme does not imply GPUI can
//! paint every CSS color accepted by the web definition schema.

mod adapt;
mod catalog;
mod color;
mod definition;
mod paint;
mod vscode;

pub(crate) use catalog::catalog_base_name;
pub(crate) use paint::{Color as CssColor, PaintError, css_color_context};

use std::collections::HashMap;

use serde_json::Value;

pub(crate) const MAX_THEME_IMPORT_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImportError {
    Invalid,
    Size,
    Include,
    Background,
}

impl ImportError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Size => "size",
            Self::Include => "include",
            Self::Background => "background",
        }
    }
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for ImportError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentPaletteRole {
    pub(crate) index: usize,
    pub(crate) colorful: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedTheme {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) variant: String,
    pub(crate) metadata: Value,
    pub(crate) colors: HashMap<String, String>,
    pub(crate) agent_palette: Vec<AgentPaletteRole>,
}

/// Convert a standalone VS Code JSON/JSONC file to the compact server format.
pub(crate) fn import_vscode_theme(text: &str, filename: &str) -> Result<Value, ImportError> {
    vscode::import(text, filename)
}

/// Validate and resolve a server definition into the native resource shape.
/// Optional component roles stay optional, exactly as in `renderNativeThemes`.
pub(crate) fn resolve_theme(source: &Value) -> Result<ResolvedTheme, ImportError> {
    definition::resolve(source)
}

/// The CSS generator's opacity fallback, preserving canonical alpha replacement.
pub(crate) fn css_opacity(value: &str, alpha: f64) -> String {
    color::opacity(value, alpha)
}

/// Resolve final CSS paint without changing the canonical definition's strings.
pub(crate) fn resolve_css_color(
    value: &str,
    dark: bool,
    current_color: Option<CssColor>,
) -> Result<[f32; 4], PaintError> {
    paint::resolve_css_color(value, dark, current_color)
}

type Colors = HashMap<String, String>;

// ECMAScript's trim and regular-expression \s set differs from Rust's.
fn whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn trim(value: &str) -> &str {
    value.trim_matches(whitespace)
}

fn flatten(value: &Value, prefix: &str, colors: &mut Colors) {
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            if let Some(color) = value.as_str() {
                colors.insert(path, color.to_owned());
            } else {
                flatten(value, &path, colors);
            }
        }
    }
}

fn get(colors: &Colors, key: &str) -> String {
    colors.get(key).cloned().unwrap_or_default()
}

fn set(colors: &mut Colors, key: &str, value: impl Into<String>) {
    colors.insert(key.to_owned(), value.into());
}

fn fallback(colors: &Colors, keys: &[&str], default: &str) -> String {
    keys.iter()
        .find_map(|key| colors.get(*key))
        .cloned()
        .unwrap_or_else(|| default.to_owned())
}

fn nest(colors: &Colors) -> Value {
    fn insert(target: &mut Value, parts: &[&str], value: &str) {
        if parts.len() == 1 {
            target[parts[0]] = Value::String(value.to_owned());
        } else {
            if target.get(parts[0]).is_none() {
                target[parts[0]] = serde_json::json!({});
            }
            insert(&mut target[parts[0]], &parts[1..], value);
        }
    }
    let mut result = serde_json::json!({});
    for (key, value) in colors {
        insert(&mut result, &key.split('.').collect::<Vec<_>>(), value);
    }
    result
}

#[cfg(test)]
mod tests;
