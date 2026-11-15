//! Standalone JSONC file import and the palette mapping in `vscode/adapter.ts`.

use serde_json::{Value, json};

use super::color::{contrast, is_hex, mix, on, opacity, readable};
use super::{
    Colors, ImportError, MAX_THEME_IMPORT_BYTES, adapt, definition, fallback, get, nest, set,
};

struct Rule {
    scopes: Vec<String>,
    default_scope: bool,
    foreground: Option<String>,
}

/// Comments are replaced with spaces so a removed comment cannot join tokens.
/// Commas are removed only outside strings and immediately before a close token.
fn jsonc(text: &str) -> Result<Value, ImportError> {
    let input = text.strip_prefix('\u{feff}').unwrap_or(text).as_bytes();
    let mut output = input.to_vec();
    let (mut at, mut string, mut escaped) = (0, false, false);
    while at < input.len() {
        let byte = input[at];
        if string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                string = false;
            }
            at += 1;
            continue;
        }
        if byte == b'"' {
            string = true;
            at += 1;
            continue;
        }
        if byte == b'/' && input.get(at + 1) == Some(&b'/') {
            while at < input.len() && input[at] != b'\n' {
                output[at] = b' ';
                at += 1;
            }
        } else if byte == b'/' && input.get(at + 1) == Some(&b'*') {
            output[at] = b' ';
            output[at + 1] = b' ';
            at += 2;
            let mut closed = false;
            while at < input.len() {
                if input[at] == b'*' && input.get(at + 1) == Some(&b'/') {
                    output[at] = b' ';
                    output[at + 1] = b' ';
                    at += 2;
                    closed = true;
                    break;
                }
                if !matches!(input[at], b'\n' | b'\r') {
                    output[at] = b' ';
                }
                at += 1;
            }
            // strip-json-comments retains an unterminated block comment, so
            // JSON.parse rejects it. A final line comment is allowed.
            if !closed {
                return Err(ImportError::Invalid);
            }
        } else {
            at += 1;
        }
    }
    let (mut string, mut escaped) = (false, false);
    for at in 0..output.len() {
        let byte = output[at];
        if string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                string = false;
            }
        } else if byte == b'"' {
            string = true;
        } else if byte == b',' {
            let next = output[at + 1..]
                .iter()
                .find(|byte| !byte.is_ascii_whitespace());
            if next.is_some_and(|byte| matches!(byte, b'}' | b']')) {
                output[at] = b' ';
            }
        }
    }
    serde_json::from_slice(&output).map_err(|_| ImportError::Invalid)
}

fn optional_string<'a>(
    source: &'a Value,
    key: &str,
    limit: Option<usize>,
) -> Result<Option<&'a str>, ImportError> {
    match source.get(key) {
        None => Ok(None),
        Some(value) => {
            let text = value.as_str().ok_or(ImportError::Invalid)?;
            if limit.is_some_and(|limit| text.encode_utf16().count() > limit) {
                return Err(ImportError::Invalid);
            }
            Ok(Some(text))
        }
    }
}

fn token_foreground(value: &Value) -> Result<Option<String>, ImportError> {
    let object = value.as_object().ok_or(ImportError::Invalid)?;
    match object.get("foreground") {
        None => Ok(None),
        Some(value) => {
            let value = value.as_str().ok_or(ImportError::Invalid)?;
            if value.is_empty() {
                Ok(None)
            } else if is_hex(value) {
                Ok(Some(value.to_owned()))
            } else {
                Err(ImportError::Invalid)
            }
        }
    }
}

fn rules(source: &Value) -> Result<Option<Vec<Rule>>, ImportError> {
    let Some(value) = source.get("tokenColors") else {
        return Ok(Some(Vec::new()));
    };
    if value.is_string() {
        return Ok(None);
    }
    let entries = value.as_array().ok_or(ImportError::Invalid)?;
    let mut rules = Vec::new();
    for entry in entries {
        let object = entry.as_object().ok_or(ImportError::Invalid)?;
        let scopes = match object.get("scope") {
            None => vec![String::new()],
            Some(Value::String(scope)) => vec![scope.clone()],
            Some(Value::Array(scopes)) => scopes
                .iter()
                .map(|scope| {
                    scope
                        .as_str()
                        .map(str::to_owned)
                        .ok_or(ImportError::Invalid)
                })
                .collect::<Result<_, _>>()?,
            _ => return Err(ImportError::Invalid),
        };
        let default_scope = object.get("scope").is_none()
            || object.get("scope").and_then(Value::as_str) == Some("");
        rules.push(Rule {
            scopes: scopes
                .into_iter()
                .flat_map(|scope| {
                    scope
                        .split(',')
                        .map(|part| super::trim(part).to_owned())
                        .collect::<Vec<_>>()
                })
                .collect(),
            default_scope,
            foreground: token_foreground(&entry["settings"])?,
        });
    }
    Ok(Some(rules))
}

fn textmate(rules: &[Rule], candidates: &[&str]) -> Option<String> {
    for candidate in candidates {
        let mut best = None;
        let mut specificity = 0;
        for rule in rules {
            let Some(color) = &rule.foreground else {
                continue;
            };
            for scope in &rule.scopes {
                if !scope.is_empty()
                    && (*candidate == scope || candidate.starts_with(&format!("{scope}.")))
                    && scope.len() >= specificity
                {
                    best = Some(color.clone());
                    specificity = scope.len();
                }
            }
        }
        if best.is_some() {
            return best;
        }
    }
    None
}

fn pick(
    semantic: &Colors,
    rules: &[Rule],
    semantics: &[&str],
    scopes: &[&str],
    default: &str,
) -> String {
    semantics
        .iter()
        .chain((!semantics.is_empty()).then_some(&"*"))
        .find_map(|key| semantic.get(*key))
        .cloned()
        .or_else(|| textmate(rules, scopes))
        .unwrap_or_else(|| default.to_owned())
}

fn name(source: Option<&str>, filename: &str) -> Result<String, ImportError> {
    let text = match source {
        Some(name) => super::trim(name).to_owned(),
        None => {
            let mut name = filename;
            for suffix in [".json", ".jsonc", ".code-theme"] {
                if name.to_ascii_lowercase().ends_with(suffix) {
                    name = &name[..name.len() - suffix.len()];
                    break;
                }
            }
            for suffix in [
                "-color-theme",
                "-color_theme",
                "_color-theme",
                "_color_theme",
            ] {
                if name.to_ascii_lowercase().ends_with(suffix) {
                    name = &name[..name.len() - suffix.len()];
                    break;
                }
            }
            let text = String::from_utf16(
                &super::trim(name)
                    .encode_utf16()
                    .take(160)
                    .collect::<Vec<_>>(),
            )
            .map_err(|_| ImportError::Invalid)?;
            if text.is_empty() {
                "VS Code".to_owned()
            } else {
                text
            }
        }
    };
    let parts: Vec<_> = text.split(['-', '_']).collect();
    if parts.len() > 1
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
    {
        Ok(parts
            .iter()
            .map(|word| format!("{}{}", word[..1].to_ascii_uppercase(), &word[1..]))
            .collect::<Vec<_>>()
            .join(" "))
    } else {
        Ok(text)
    }
}

pub(super) fn import(text: &str, filename: &str) -> Result<Value, ImportError> {
    if text.len() > MAX_THEME_IMPORT_BYTES {
        return Err(ImportError::Size);
    }
    let source = jsonc(text)?;
    source.as_object().ok_or(ImportError::Invalid)?;
    let source_name = optional_string(&source, "name", None)?;
    if source_name.is_some_and(|name| {
        super::trim(name).is_empty() || super::trim(name).encode_utf16().count() > 160
    }) {
        return Err(ImportError::Invalid);
    }
    let author = optional_string(&source, "author", Some(160))?;
    let description = optional_string(&source, "description", Some(1024))?;
    let kind = optional_string(&source, "type", None)?;
    if kind.is_some_and(|kind| !matches!(kind, "dark" | "light" | "hc" | "hc-black" | "hc-light")) {
        return Err(ImportError::Invalid);
    }
    let include = optional_string(&source, "include", None)?;
    let mut colors = Colors::new();
    if let Some(values) = source.get("colors") {
        for (key, value) in values.as_object().ok_or(ImportError::Invalid)? {
            if let Some(value) = value.as_str().filter(|value| is_hex(value)) {
                set(&mut colors, key, value);
            }
        }
    }
    let token_rules = rules(&source)?;
    let semantic_enabled = match source.get("semanticHighlighting") {
        None => true,
        Some(value) => value.as_bool().ok_or(ImportError::Invalid)?,
    };
    let mut semantic = Colors::new();
    if let Some(value) = source.get("semanticTokenColors") {
        for (key, value) in value.as_object().ok_or(ImportError::Invalid)? {
            let foreground = if let Some(text) = value.as_str() {
                if !is_hex(text) {
                    return Err(ImportError::Invalid);
                }
                Some(text.to_owned())
            } else {
                token_foreground(value)?
            };
            if semantic_enabled && let Some(foreground) = foreground {
                set(&mut semantic, key, foreground);
            }
        }
    }
    if include.is_some_and(|value| !value.is_empty()) || token_rules.is_none() {
        return Err(ImportError::Include);
    }
    let rules = token_rules.unwrap_or_default();
    let authored_background = colors
        .get("editor.background")
        .ok_or(ImportError::Background)?
        .clone();
    let dark = kind.map_or_else(
        || on(&authored_background, "#ffffff") == "#ffffff",
        |kind| matches!(kind, "dark" | "hc" | "hc-black"),
    );
    let high_contrast = kind.is_some_and(|kind| kind.starts_with("hc"));
    let variant = if dark { "dark" } else { "light" };
    let background = mix(
        &authored_background,
        if dark { "#000000" } else { "#ffffff" },
        1.0,
        "#ffffff",
    );
    let default_token = rules
        .iter()
        .filter(|rule| rule.default_scope)
        .filter_map(|rule| rule.foreground.as_ref())
        .next_back();
    let foreground = colors
        .get("editor.foreground")
        .or(default_token)
        .or_else(|| colors.get("foreground"))
        .cloned()
        .unwrap_or_else(|| on(&background, &background));
    set(&mut colors, "editor.background", &background);
    set(&mut colors, "editor.foreground", &foreground);
    if high_contrast && let Some(border) = colors.get("contrastBorder").cloned() {
        set(&mut colors, "widget.border", border);
    }
    let mut mapped = map_palette(&colors, dark, high_contrast)?;
    let code_foreground = get(&mapped, "syntax.base.foreground");
    for (key, semantics, scopes, fallback) in [
        (
            "comment",
            &["comment"][..],
            &["comment", "comment.line", "comment.block"][..],
            readable(
                &mix(&foreground, &background, 0.6, &background),
                &background,
                "#ffffff",
            ),
        ),
        (
            "keyword",
            &["keyword"][..],
            &["keyword", "storage", "keyword.control", "storage.type"][..],
            code_foreground.clone(),
        ),
        (
            "string",
            &["string"][..],
            &["string", "string.quoted", "string.template"][..],
            code_foreground.clone(),
        ),
        (
            "number",
            &["number"][..],
            &["constant.numeric"][..],
            code_foreground.clone(),
        ),
        (
            "function",
            &["function"][..],
            &["entity.name.function", "support.function"][..],
            code_foreground.clone(),
        ),
        (
            "variable",
            &["variable"][..],
            &["variable.other.readwrite", "variable"][..],
            code_foreground.clone(),
        ),
        (
            "type",
            &["type"][..],
            &["entity.name.type", "support.type", "support.class"][..],
            code_foreground.clone(),
        ),
        (
            "operator",
            &["operator"][..],
            &["keyword.operator"][..],
            code_foreground,
        ),
    ] {
        set(
            &mut mapped,
            &format!("syntax.base.{key}"),
            pick(&semantic, &rules, semantics, scopes, &fallback),
        );
    }
    let base = |key: &str| get(&mapped, &format!("syntax.base.{key}"));
    let class = pick(
        &semantic,
        &rules,
        &["class"],
        &["entity.name.type.class", "entity.name.class"],
        &base("type"),
    );
    let property = pick(
        &semantic,
        &rules,
        &["property"],
        &["variable.other.property", "support.type.property-name"],
        &base("variable"),
    );
    let tokens = [
        ("className", class.clone()),
        (
            "interface",
            pick(
                &semantic,
                &rules,
                &["interface"],
                &["entity.name.type.interface"],
                &base("type"),
            ),
        ),
        (
            "enum",
            pick(
                &semantic,
                &rules,
                &["enum"],
                &["entity.name.type.enum"],
                &class,
            ),
        ),
        (
            "method",
            pick(
                &semantic,
                &rules,
                &["method"],
                &["entity.name.function.method"],
                &base("function"),
            ),
        ),
        ("variableProperty", property.clone()),
        (
            "parameter",
            pick(
                &semantic,
                &rules,
                &["parameter"],
                &["variable.parameter"],
                &base("variable"),
            ),
        ),
        (
            "constant",
            pick(
                &semantic,
                &rules,
                &["variable.readonly", "*.readonly", "enumMember"],
                &["variable.other.constant", "constant.other"],
                &base("number"),
            ),
        ),
        (
            "boolean",
            pick(
                &semantic,
                &rules,
                &[],
                &["constant.language.boolean", "constant.language"],
                &base("number"),
            ),
        ),
        (
            "punctuation",
            pick(
                &semantic,
                &rules,
                &[],
                &["punctuation", "punctuation.separator"],
                &base("operator"),
            ),
        ),
        (
            "stringEscape",
            pick(
                &semantic,
                &rules,
                &[],
                &["constant.character.escape"],
                &base("string"),
            ),
        ),
        (
            "commentDoc",
            pick(
                &semantic,
                &rules,
                &[],
                &["comment.block.documentation", "comment.documentation"],
                &base("comment"),
            ),
        ),
        (
            "keywordImport",
            pick(
                &semantic,
                &rules,
                &[],
                &["keyword.control.import", "keyword.import"],
                &base("keyword"),
            ),
        ),
        (
            "tag",
            pick(
                &semantic,
                &rules,
                &[],
                &["entity.name.tag"],
                &base("keyword"),
            ),
        ),
        (
            "tagAttribute",
            pick(
                &semantic,
                &rules,
                &[],
                &["entity.other.attribute-name"],
                &property,
            ),
        ),
    ];
    for (key, value) in tokens {
        set(&mut mapped, &format!("syntax.tokens.{key}"), value);
    }
    let mut metadata = json!({"id": format!("vscode-import-{variant}"), "name": name(source_name, filename)?, "variant": variant, "description": description.unwrap_or(""), "version": "1.0.0", "tags": ["imported", "vscode"]});
    if let Some(author) = author {
        metadata["author"] = Value::String(author.to_owned());
    }
    definition::compact(&json!({"metadata": metadata, "colors": nest(&mapped)}))
}

fn has_paint(value: &str) -> bool {
    value != "transparent"
        && (contrast(value, "#000000", "#ffffff") != Some(1.0)
            || contrast(value, "#ffffff", "#ffffff") != Some(1.0))
}

fn map_palette(authored: &Colors, dark: bool, high_contrast: bool) -> Result<Colors, ImportError> {
    // These are already generated by the canonical default-theme resolver.
    let resources: Value = serde_json::from_str(include_str!("../../resources/themes.json"))
        .map_err(|_| ImportError::Invalid)?;
    let id = if dark {
        "openchamber-dark"
    } else {
        "openchamber-light"
    };
    let entry = resources["themes"]
        .as_array()
        .and_then(|themes| themes.iter().find(|theme| theme["id"] == id))
        .ok_or(ImportError::Invalid)?;
    let defaults: Colors =
        serde_json::from_value(entry["colors"].clone()).map_err(|_| ImportError::Invalid)?;
    let mut colors = authored.clone();
    let canvas = fallback(
        &colors,
        &["chat.list.background", "editor.background"],
        &get(&defaults, "surface.background"),
    );
    let text = fallback(
        &colors,
        &[
            "interactive-session.foreground",
            "foreground",
            "editor.foreground",
        ],
        &on(&canvas, &canvas),
    );
    colors
        .entry("foreground".to_owned())
        .or_insert(text.clone());
    let sidebar = fallback(
        &colors,
        &["panel.background"],
        &mix(&text, &canvas, 0.03, &canvas),
    );
    colors
        .entry("sideBar.background".to_owned())
        .or_insert(sidebar);
    if ![
        "editorWidget.background",
        "dropdown.background",
        "input.background",
    ]
    .iter()
    .any(|key| colors.contains_key(*key))
    {
        let widget = mix(&text, &canvas, 0.04, &canvas);
        set(
            &mut colors,
            "editorWidget.foreground",
            readable(&text, &widget, &canvas),
        );
        set(&mut colors, "editorWidget.background", widget);
    }
    colors
        .entry("descriptionForeground".to_owned())
        .or_insert_with(|| readable(&mix(&text, &canvas, 0.6, &canvas), &canvas, "#ffffff"));
    for (key, value) in [
        (
            "widget.border",
            fallback(
                &colors,
                &["input.border", "panel.border"],
                &mix(&text, &canvas, 0.15, &canvas),
            ),
        ),
        (
            "toolbar.hoverBackground",
            fallback(
                &colors,
                &["list.hoverBackground"],
                &mix(&text, &canvas, 0.08, &canvas),
            ),
        ),
        (
            "toolbar.activeBackground",
            mix(&text, &canvas, 0.12, &canvas),
        ),
    ] {
        colors.entry(key.to_owned()).or_insert(value);
    }
    if ![
        "list.activeSelectionBackground",
        "menu.selectionBackground",
        "editor.selectionBackground",
    ]
    .iter()
    .any(|key| colors.contains_key(*key))
    {
        let selection = mix(&text, &canvas, 0.16, &canvas);
        set(
            &mut colors,
            "list.activeSelectionForeground",
            readable(&text, &selection, &canvas),
        );
        set(&mut colors, "list.activeSelectionBackground", selection);
    }
    if !colors.contains_key("button.background") {
        let button = fallback(&colors, &["textLink.foreground", "focusBorder"], &text);
        set(&mut colors, "button.foreground", on(&button, &canvas));
        set(&mut colors, "button.background", button);
    }
    let button = get(&colors, "button.background");
    colors.entry("focusBorder".to_owned()).or_insert(button);
    let cursor = fallback(&colors, &["editor.foreground"], &text);
    colors
        .entry("editorCursor.foreground".to_owned())
        .or_insert(cursor);
    let read = |keys: &[&str], default: &str| fallback(&colors, keys, default);
    let foreground = read(
        &[
            "interactive-session.foreground",
            "foreground",
            "editor.foreground",
        ],
        &get(&defaults, "surface.foreground"),
    );
    let elevated_source = [
        ("editorWidget.background", "editorWidget.foreground"),
        ("dropdown.background", "dropdown.foreground"),
        ("input.background", "input.foreground"),
    ]
    .into_iter()
    .find(|(key, _)| colors.contains_key(*key));
    let elevated = elevated_source.map_or_else(
        || get(&defaults, "surface.elevated"),
        |(key, _)| read(&[key], &get(&defaults, "surface.elevated")),
    );
    let elevated_fallback = readable(&foreground, &elevated, &canvas);
    let elevated_text = elevated_source.map_or_else(
        || elevated_fallback.clone(),
        |(_, key)| read(&[key], &elevated_fallback),
    );
    let muted = read(
        &["sideBar.background", "panel.background"],
        &get(&defaults, "surface.muted"),
    );
    let muted_text = read(
        &["descriptionForeground", "input.placeholderForeground"],
        &get(&defaults, "surface.mutedForeground"),
    );
    let accent = read(
        &["button.background", "textLink.foreground"],
        &get(&defaults, "primary.base"),
    );
    let accent_hover = read(&["button.hoverBackground"], &accent);
    let accent_foreground = read(&["button.foreground"], &on(&accent, &canvas));
    let effective_border = (if high_contrast {
        colors.get("contrastBorder")
    } else {
        None
    })
    .into_iter()
    .chain(
        [
            "widget.border",
            "input.border",
            "panel.border",
            "sideBar.border",
            "editorWidget.border",
            "contrastBorder",
        ]
        .iter()
        .filter_map(|key| colors.get(*key)),
    )
    .find(|value| has_paint(value))
    .cloned()
    .unwrap_or_else(|| opacity(&foreground, if dark { 0.25 } else { 0.2 }));
    let candidates: Vec<_> = [
        (
            "list.activeSelectionBackground",
            "list.activeSelectionForeground",
        ),
        ("menu.selectionBackground", "menu.selectionForeground"),
        ("editor.selectionBackground", "editor.selectionForeground"),
    ]
    .into_iter()
    .filter(|(key, _)| colors.contains_key(*key))
    .collect();
    let selected = candidates
        .iter()
        .find(|(key, _)| {
            [&canvas, &muted, &elevated].iter().all(|surface| {
                contrast(&get(&colors, key), surface, &canvas)
                    .is_none_or(|contrast| contrast > 1.01)
            })
        })
        .or_else(|| candidates.first());
    let selection = selected.map_or_else(
        || get(&defaults, "interactive.selection"),
        |(key, _)| read(&[key], &get(&defaults, "interactive.selection")),
    );
    let selection_foreground =
        selected.map_or_else(|| foreground.clone(), |(_, key)| read(&[key], &foreground));
    let focus = read(&["focusBorder", "inputOption.activeBorder"], &accent);
    let mut mapped = Colors::new();
    for (key, value) in [
        ("surface.background", canvas.clone()),
        ("surface.foreground", foreground.clone()),
        ("surface.muted", muted.clone()),
        ("surface.mutedForeground", muted_text.clone()),
        ("surface.elevated", elevated.clone()),
        ("surface.elevatedForeground", elevated_text),
        ("surface.overlay", get(&defaults, "surface.overlay")),
        (
            "surface.subtle",
            read(&["textBlockQuote.background"], &muted),
        ),
        ("primary.base", accent.clone()),
        ("primary.hover", accent_hover.clone()),
        ("primary.active", accent_hover.clone()),
        ("primary.foreground", accent_foreground.clone()),
        ("primary.muted", opacity(&accent, 0.5)),
        ("interactive.border", effective_border.clone()),
        (
            "interactive.borderHover",
            read(&["toolbar.hoverOutline"], &effective_border),
        ),
        ("interactive.borderFocus", focus.clone()),
        ("interactive.selection", selection),
        ("interactive.selectionForeground", selection_foreground),
        ("interactive.focus", focus.clone()),
        ("interactive.focusRing", focus),
        (
            "interactive.cursor",
            read(
                &["editorCursor.foreground"],
                &get(&defaults, "interactive.cursor"),
            ),
        ),
        (
            "interactive.hover",
            read(
                &["toolbar.hoverBackground", "list.hoverBackground"],
                &get(&defaults, "interactive.hover"),
            ),
        ),
        (
            "interactive.active",
            read(
                &[
                    "toolbar.activeBackground",
                    "toolbar.hoverBackground",
                    "list.hoverBackground",
                ],
                &get(&defaults, "interactive.hover"),
            ),
        ),
    ] {
        set(&mut mapped, key, value);
    }
    for (role, keys, bg_key) in [
        (
            "error",
            &["editorError.foreground", "testing.iconFailed"][..],
            "editorError.background",
        ),
        (
            "warning",
            &[
                "problemsWarningIcon.foreground",
                "notificationsWarningIcon.foreground",
                "editorWarning.foreground",
            ][..],
            "editorWarning.background",
        ),
        (
            "success",
            &[
                "testing.iconPassed",
                "gitDecoration.addedResourceForeground",
            ][..],
            "",
        ),
        (
            "info",
            &["problemsInfoIcon.foreground", "editorInfo.foreground"][..],
            "editorInfo.background",
        ),
    ] {
        let color = read(keys, &get(&defaults, &format!("status.{role}")));
        let fallback_bg = opacity(&color, if dark { 0.16 } else { 0.12 });
        let bg = read(&[bg_key], &fallback_bg);
        set(&mut mapped, &format!("status.{role}"), &color);
        set(
            &mut mapped,
            &format!("status.{role}Foreground"),
            on(&color, &canvas),
        );
        set(
            &mut mapped,
            &format!("status.{role}Background"),
            if has_paint(&bg) { bg } else { fallback_bg },
        );
        set(
            &mut mapped,
            &format!("status.{role}Border"),
            opacity(&color, if dark { 0.45 } else { 0.35 }),
        );
    }
    for (role, value) in [
        ("open", get(&mapped, "status.success")),
        ("closed", get(&mapped, "status.error")),
        ("draft", muted_text.clone()),
        ("blocked", get(&defaults, "pr.blocked")),
        ("merged", get(&defaults, "pr.merged")),
    ] {
        set(&mut mapped, &format!("pr.{role}"), value);
    }
    let syntax_comment = read(&["editorLineNumber.foreground"], &muted_text);
    let syntax_string = read(
        &["textPreformat.foreground", "terminal.ansiGreen"],
        &get(&defaults, "syntax.base.string"),
    );
    for (key, value) in [
        ("background", read(&["editor.background"], &canvas)),
        ("foreground", read(&["editor.foreground"], &foreground)),
        ("comment", syntax_comment.clone()),
        ("string", syntax_string.clone()),
        ("keyword", read(&["terminal.ansiBlue"], &accent)),
        (
            "number",
            read(
                &["terminal.ansiYellow"],
                &get(&defaults, "syntax.base.number"),
            ),
        ),
        (
            "function",
            read(
                &["terminal.ansiCyan"],
                &get(&defaults, "syntax.base.function"),
            ),
        ),
        ("variable", read(&["terminal.ansiMagenta"], &foreground)),
        (
            "type",
            read(
                &["terminal.ansiYellow"],
                &get(&defaults, "syntax.base.type"),
            ),
        ),
        ("operator", accent.clone()),
    ] {
        set(&mut mapped, &format!("syntax.base.{key}"), value);
    }
    let added = read(
        &[
            "chat.linesAddedForeground",
            "gitDecoration.addedResourceForeground",
        ],
        &get(&mapped, "status.success"),
    );
    let removed = read(
        &[
            "chat.linesRemovedForeground",
            "gitDecoration.deletedResourceForeground",
        ],
        &get(&mapped, "status.error"),
    );
    let modified = read(
        &["gitDecoration.modifiedResourceForeground"],
        &get(&mapped, "status.info"),
    );
    let added_bg = read(
        &[
            "diffEditor.insertedLineBackground",
            "diffEditor.insertedTextBackground",
        ],
        &get(&mapped, "status.successBackground"),
    );
    let removed_bg = read(
        &[
            "diffEditor.removedLineBackground",
            "diffEditor.removedTextBackground",
        ],
        &get(&mapped, "status.errorBackground"),
    );
    let modified_bg = opacity(&modified, if dark { 0.16 } else { 0.12 });
    for (key, value) in [
        ("diffAdded", added),
        ("diffRemoved", removed),
        ("diffModified", modified),
        ("diffAddedBackground", added_bg.clone()),
        ("diffRemovedBackground", removed_bg.clone()),
        ("diffModifiedBackground", modified_bg.clone()),
        (
            "lineNumber",
            read(&["editorLineNumber.foreground"], &muted_text),
        ),
        (
            "lineNumberActive",
            read(&["editorLineNumber.activeForeground"], &foreground),
        ),
    ] {
        set(&mut mapped, &format!("syntax.highlights.{key}"), value);
    }
    for (key, value) in [
        (
            "tools.border",
            read(&["chat.requestBorder"], &effective_border),
        ),
        ("tools.icon", read(&["icon.foreground"], &muted_text)),
        ("tools.title", foreground.clone()),
        ("tools.description", opacity(&muted_text, 0.8)),
        ("tools.edit.addedBackground", added_bg),
        ("tools.edit.removedBackground", removed_bg),
        ("tools.edit.modifiedBackground", modified_bg),
        ("tools.edit.lineNumber", syntax_comment),
        ("chat.background", canvas.clone()),
        (
            "chat.userMessageBackground",
            read(
                &["chat.requestBubbleBackground", "chat.requestBackground"],
                &elevated,
            ),
        ),
        ("chat.divider", effective_border.clone()),
        ("markdown.link", read(&["textLink.foreground"], &accent)),
        (
            "markdown.linkHover",
            read(&["textLink.activeForeground"], &accent_hover),
        ),
        (
            "markdown.inlineCode",
            read(&["textPreformat.foreground"], &syntax_string),
        ),
        (
            "markdown.inlineCodeBackground",
            read(&["textPreformat.background", "editor.background"], &canvas),
        ),
        ("markdown.blockquote", foreground),
        (
            "markdown.blockquoteBorder",
            read(&["textBlockQuote.border"], &effective_border),
        ),
        ("markdown.listMarker", opacity(&accent, 0.6)),
    ] {
        set(&mut mapped, key, value);
    }
    adapt::apply(&mut mapped, authored, dark, high_contrast);
    Ok(mapped)
}
