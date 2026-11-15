//! `definition.ts`, `syntax.ts`, `readableColors.ts` and `agentColorPalette.ts`.

use serde_json::{Map, Value, json};

use super::color::{contrast, distance, mix, on, opacity, readable};
use super::{AgentPaletteRole, Colors, ImportError, ResolvedTheme, flatten, get, set};

const PRIMARY: &[&str] = &["base", "foreground", "hover", "active", "muted"];
const SURFACE: &[&str] = &[
    "background",
    "foreground",
    "muted",
    "mutedForeground",
    "elevated",
    "elevatedForeground",
    "overlay",
    "subtle",
];
const INTERACTIVE: &[&str] = &[
    "border",
    "selection",
    "selectionForeground",
    "borderHover",
    "borderFocus",
    "focus",
    "focusRing",
    "cursor",
    "hover",
    "active",
];
const STATUS: &[&str] = &[
    "error",
    "errorForeground",
    "errorBackground",
    "errorBorder",
    "warning",
    "warningForeground",
    "warningBackground",
    "warningBorder",
    "success",
    "successForeground",
    "successBackground",
    "successBorder",
    "info",
    "infoForeground",
    "infoBackground",
    "infoBorder",
];
const BASE: &[&str] = &[
    "comment",
    "keyword",
    "string",
    "number",
    "function",
    "variable",
    "type",
    "operator",
    "background",
    "foreground",
];

fn object(value: &Value) -> Result<&Map<String, Value>, ImportError> {
    value.as_object().ok_or(ImportError::Invalid)
}

fn string(value: &Value) -> Result<&str, ImportError> {
    value.as_str().ok_or(ImportError::Invalid)
}

fn nonempty(value: &Value) -> Result<String, ImportError> {
    let text = super::trim(string(value)?);
    if text.is_empty() {
        Err(ImportError::Invalid)
    } else {
        Ok(text.to_owned())
    }
}

fn loads_resource(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("@import") {
        return true;
    }
    for name in ["url", "image", "image-set", "cross-fade", "element"] {
        for (at, _) in lower.match_indices(name) {
            let boundary = at == 0
                || !lower.as_bytes()[at - 1].is_ascii_alphanumeric()
                    && lower.as_bytes()[at - 1] != b'_';
            if boundary
                && lower[at + name.len()..]
                    .trim_start_matches(super::whitespace)
                    .starts_with('(')
            {
                return true;
            }
        }
    }
    false
}

fn color(value: &Value) -> Result<Value, ImportError> {
    let text = nonempty(value)?;
    let valid = if let Some(hex) = text.strip_prefix('#') {
        (3..=8).contains(&hex.len()) && hex.bytes().all(|c| c.is_ascii_hexdigit())
    } else if text.bytes().all(|c| c.is_ascii_alphabetic()) {
        true
    } else if let Some((name, body)) = text.split_once('(') {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "rgb"
                | "rgba"
                | "hsl"
                | "hsla"
                | "hwb"
                | "lab"
                | "lch"
                | "oklab"
                | "oklch"
                | "color"
                | "color-mix"
                | "light-dark"
        ) && body.strip_suffix(')').is_some_and(|body| {
            body.chars().all(|c| {
                c.is_ascii_alphanumeric() || super::whitespace(c) || "_#.,%/+()-".contains(c)
            })
        })
    } else {
        false
    };
    if valid && !loads_resource(&text) {
        Ok(Value::String(text))
    } else {
        Err(ImportError::Invalid)
    }
}

fn colors(
    value: &Value,
    allowed: &[&str],
    required: &[&str],
    record: bool,
) -> Result<Value, ImportError> {
    let source = object(value)?;
    let mut result = Map::new();
    for key in required {
        if !source.contains_key(*key) {
            return Err(ImportError::Invalid);
        }
    }
    for (key, value) in source {
        if record || allowed.contains(&key.as_str()) {
            let parsed = color(value)?;
            if allowed.is_empty() || allowed.contains(&key.as_str()) {
                result.insert(key.clone(), parsed);
            }
        }
    }
    Ok(Value::Object(result))
}

pub(super) fn validate(value: &Value) -> Result<Value, ImportError> {
    object(value)?;
    let source = object(&value["metadata"])?;
    let mut metadata = json!({"id": nonempty(&value["metadata"]["id"])?, "name": nonempty(&value["metadata"]["name"])?, "variant": string(&value["metadata"]["variant"])?});
    if !matches!(metadata["variant"].as_str(), Some("dark" | "light")) {
        return Err(ImportError::Invalid);
    }
    for (key, default) in [("description", ""), ("version", "1.0.0")] {
        metadata[key] = Value::String(match source.get(key) {
            Some(value) => string(value)?.to_owned(),
            None => default.to_owned(),
        });
    }
    if let Some(value) = source.get("author") {
        metadata["author"] = Value::String(string(value)?.to_owned());
    }
    metadata["tags"] = match source.get("tags") {
        Some(value) => {
            let tags = value.as_array().ok_or(ImportError::Invalid)?;
            for tag in tags {
                string(tag)?;
            }
            value.clone()
        }
        None => json!([]),
    };
    let c = object(&value["colors"])?;
    let mut result = json!({"metadata": metadata, "colors": {
        "primary": colors(&value["colors"]["primary"], PRIMARY, &["base"], false)?,
        "surface": colors(&value["colors"]["surface"], SURFACE, &SURFACE[..5], false)?,
        "interactive": colors(&value["colors"]["interactive"], INTERACTIVE, &["border"], false)?,
        "status": colors(&value["colors"]["status"], STATUS, &["error", "warning", "success", "info"], false)?,
        "syntax": {"base": colors(&value["colors"]["syntax"]["base"], BASE, &BASE[..8], false)?}
    }});
    let syntax = object(&value["colors"]["syntax"])?;
    for key in ["tokens", "highlights"] {
        if let Some(value) = syntax.get(key) {
            result["colors"]["syntax"][key] = colors(value, &[], &[], true)?;
        }
    }
    for (group, keys, record) in [
        (
            "pr",
            &["open", "draft", "blocked", "merged", "closed"][..],
            false,
        ),
        (
            "chat",
            &["background", "userMessageBackground", "divider"][..],
            true,
        ),
        (
            "markdown",
            &[
                "link",
                "linkHover",
                "inlineCode",
                "inlineCodeBackground",
                "blockquote",
                "blockquoteBorder",
                "listMarker",
                "bold",
                "italic",
                "strikethrough",
                "hr",
            ][..],
            true,
        ),
    ] {
        if let Some(value) = c.get(group) {
            result["colors"][group] = colors(value, keys, &[], record)?;
        }
    }
    if let Some(value) = c.get("tools") {
        result["colors"]["tools"] = colors(
            value,
            &["border", "icon", "title", "description"],
            &[],
            false,
        )?;
        if let Some(edit) = value.get("edit") {
            result["colors"]["tools"]["edit"] = colors(
                edit,
                &[
                    "addedBackground",
                    "removedBackground",
                    "modifiedBackground",
                    "lineNumber",
                ],
                &[],
                true,
            )?;
        }
    }
    if let Some(config) = value.get("config") {
        object(config)?;
        let mut output = json!({});
        for (group, keys) in [
            ("fonts", ["sans", "mono", "heading"]),
            ("transitions", ["fast", "normal", "slow"]),
        ] {
            if let Some(value) = config.get(group) {
                let input = object(value)?;
                output[group] = json!({});
                for key in keys {
                    if let Some(value) = input.get(key) {
                        let text = string(value)?;
                        if text.chars().any(|c| ";{}<>\\".contains(c)) || loads_resource(text) {
                            return Err(ImportError::Invalid);
                        }
                        output[group][key] = value.clone();
                    }
                }
            }
        }
        result["config"] = output;
    }
    Ok(result)
}

pub(super) fn syntax_tokens(base: &Colors, overrides: &Colors) -> Colors {
    let b = |key: &str| get(base, key);
    let t = |key: &str, fallback: &str| {
        overrides
            .get(key)
            .cloned()
            .unwrap_or_else(|| fallback.to_owned())
    };
    let property = t("variableProperty", &b("variable"));
    let class = t("className", &t("class", &b("type")));
    let mut result = Colors::new();
    for (keys, value) in [
        ("commentDoc punctuation", b("comment")),
        ("stringEscape bracket", b("foreground")),
        (
            "stringInterpolation variableBuiltin variableOther variableLocal",
            b("variable"),
        ),
        (
            "stringRegex variableGlobal tagAttributeValue regex url",
            b("string"),
        ),
        (
            "keywordControl keywordReturn storageModifier boolean tag macro",
            b("keyword"),
        ),
        (
            "keywordOperator keywordImport module label preprocessor exception",
            b("operator"),
        ),
        (
            "functionCall functionBuiltin method decorator annotation",
            b("function"),
        ),
        ("methodCall", t("method", &b("function"))),
        ("variableProperty tagAttribute key", property),
        ("variableReadonly null constant", b("number")),
        ("parameter", t("stringEscape", &b("foreground"))),
        ("typePrimitive interface typeParameter namespace", b("type")),
        ("typeInterface", t("interface", &b("type"))),
        ("class className struct enum", class),
        ("delimiter tagBracket", t("punctuation", &b("comment"))),
        ("jsxTag", t("tag", &b("keyword"))),
    ] {
        for key in keys.split(' ') {
            set(&mut result, key, &value);
        }
    }
    result.extend(overrides.clone());
    result
}

fn group(colors: &Colors, prefix: &str) -> Colors {
    colors
        .iter()
        .filter_map(|(key, value)| {
            key.strip_prefix(prefix)
                .map(|key| (key.to_owned(), value.clone()))
        })
        .collect()
}

pub(super) fn resolved_colors(source: &Value) -> Colors {
    let mut c = Colors::new();
    flatten(&source["colors"], "", &mut c);
    let neutral = if source["metadata"]["variant"] == "dark" {
        "#ffffff"
    } else {
        "#000000"
    };
    let canvas = get(&c, "surface.background");
    let foreground = get(&c, "surface.foreground");
    let muted = get(&c, "surface.muted");
    let muted_text = get(&c, "surface.mutedForeground");
    let primary = get(&c, "primary.base");
    let border = get(&c, "interactive.border");
    let mut default = |key: &str, value: String| {
        c.entry(key.to_owned()).or_insert(value);
    };
    default("surface.elevatedForeground", foreground.clone());
    default("surface.overlay", opacity("#000000", 0.6));
    default("surface.subtle", muted);
    default("interactive.selection", opacity(&foreground, 0.16));
    default("syntax.base.background", canvas.clone());
    default("syntax.base.foreground", foreground.clone());
    for role in ["error", "warning", "success", "info"] {
        let seed = get(&c, &format!("status.{role}"));
        for (suffix, value) in [
            ("Foreground", on(&seed, &canvas)),
            ("Background", opacity(&seed, 0.125)),
            ("Border", opacity(&seed, 0.314)),
        ] {
            c.entry(format!("status.{role}{suffix}")).or_insert(value);
        }
    }
    for (kind, status) in [
        ("Added", "success"),
        ("Removed", "error"),
        ("Modified", "info"),
    ] {
        let key = format!("syntax.highlights.diff{kind}");
        let seed = get(&c, &format!("status.{status}"));
        let seed = c.entry(key.clone()).or_insert(seed).clone();
        c.entry(format!("{key}Background"))
            .or_insert_with(|| opacity(&seed, 0.125));
    }
    let code_foreground = get(&c, "syntax.base.foreground");
    let selection = get(&c, "interactive.selection");
    for (key, value) in [
        ("syntax.highlights.lineNumber", muted_text.clone()),
        (
            "syntax.highlights.lineNumberActive",
            code_foreground.clone(),
        ),
        ("primary.foreground", on(&primary, &canvas)),
        ("primary.hover", mix(&primary, neutral, 0.9, "#ffffff")),
        ("primary.active", mix(&primary, neutral, 0.8, "#ffffff")),
        (
            "interactive.borderHover",
            mix(&border, &foreground, 0.7, "#ffffff"),
        ),
        ("interactive.borderFocus", primary.clone()),
        ("interactive.focus", primary.clone()),
        ("interactive.focusRing", opacity(&primary, 0.5)),
        ("interactive.cursor", code_foreground),
        ("interactive.hover", opacity(&foreground, 0.07)),
        ("interactive.active", opacity(&foreground, 0.12)),
        (
            "interactive.selectionForeground",
            readable(&foreground, &selection, &canvas),
        ),
        ("pr.open", get(&c, "status.success")),
        ("pr.draft", muted_text),
        ("pr.blocked", get(&c, "status.warning")),
        (
            "pr.merged",
            if neutral == "#ffffff" {
                "#8957e5"
            } else {
                "#8250df"
            }
            .to_owned(),
        ),
        ("pr.closed", get(&c, "status.error")),
    ] {
        c.entry(key.to_owned()).or_insert(value);
    }
    for (key, value) in syntax_tokens(&group(&c, "syntax.base."), &group(&c, "syntax.tokens.")) {
        set(&mut c, &format!("syntax.tokens.{key}"), value);
    }
    c
}

pub(super) fn resolve(value: &Value) -> Result<ResolvedTheme, ImportError> {
    let source = validate(value)?;
    let mut c = resolved_colors(&source);
    let metadata = source["metadata"].clone();
    let dark = metadata["variant"] == "dark";
    let canvas = get(&c, "surface.background");
    let backgrounds = [
        canvas.clone(),
        get(&c, "surface.elevated"),
        get(&c, "surface.muted"),
    ];
    for (role, path, tint) in [
        ("primary", "primary.base", if dark { 0.30 } else { 0.22 }),
        ("error", "status.error", if dark { 0.20 } else { 0.16 }),
        ("info", "status.info", 0.15),
        ("success", "status.success", 0.15),
        ("warning", "status.warning", 0.15),
    ] {
        let seed = get(&c, path);
        let mut text = seed.clone();
        for background in &backgrounds {
            text = readable(&text, &mix(&seed, background, tint, &canvas), &canvas);
        }
        set(&mut c, &format!("readable.tinted.{role}"), text);
    }
    for role in ["error", "warning", "success", "info"] {
        let text = readable(
            &get(&c, &format!("status.{role}")),
            &get(&c, &format!("status.{role}Background")),
            &canvas,
        );
        set(&mut c, &format!("readable.status.{role}"), text);
    }
    let text = readable(
        &get(&c, "interactive.selectionForeground"),
        &get(&c, "interactive.selection"),
        &canvas,
    );
    set(&mut c, "readable.selectionForeground", text);
    let agent_palette = agent_palette(&c);
    Ok(ResolvedTheme {
        id: metadata["id"].as_str().unwrap_or_default().to_owned(),
        name: metadata["name"].as_str().unwrap_or_default().to_owned(),
        variant: metadata["variant"].as_str().unwrap_or_default().to_owned(),
        metadata,
        colors: c,
        agent_palette,
    })
}

fn agent_palette(c: &Colors) -> Vec<AgentPaletteRole> {
    let canvas = get(c, "surface.background");
    let backgrounds = [
        canvas.clone(),
        get(c, "surface.muted"),
        get(c, "surface.elevated"),
    ];
    let separation = |a: &str, b: &str| {
        backgrounds
            .iter()
            .map(|bg| distance(a, b, bg, &canvas).unwrap_or(if a == b { 0.0 } else { 1.0 }))
            .fold(f64::INFINITY, f64::min)
    };
    let candidates: Vec<_> = [
        "keyword", "type", "function", "number", "string", "operator", "variable",
    ]
    .iter()
    .enumerate()
    .map(|(index, key)| (index, get(c, &format!("syntax.base.{key}"))))
    .collect();
    let visible: Vec<_> = candidates
        .iter()
        .filter(|(_, value)| {
            backgrounds
                .iter()
                .all(|bg| contrast(value, bg, &canvas).unwrap_or(1.5) >= 1.5)
        })
        .cloned()
        .collect();
    let mut remaining = if visible.is_empty() {
        candidates.clone()
    } else {
        visible.clone()
    };
    let mut palette = Vec::new();
    let mut chosen = vec![get(c, "status.success")];
    while !remaining.is_empty() {
        let (mut best, mut best_distance) = (0, -1.0);
        for (index, (_, value)) in remaining.iter().enumerate() {
            let d = chosen
                .iter()
                .map(|other| separation(value, other))
                .fold(f64::INFINITY, f64::min);
            if d > best_distance {
                best = index;
                best_distance = d;
            }
        }
        if best_distance < 0.055 {
            break;
        }
        let entry = remaining.remove(best);
        chosen.push(entry.1.clone());
        palette.push(entry);
    }
    if palette.is_empty() {
        palette.push(visible.first().unwrap_or(&candidates[0]).clone());
    }
    palette
        .into_iter()
        .map(|(index, value)| AgentPaletteRole {
            index,
            colorful: separation(&value, "#808080") >= 0.035,
        })
        .collect()
}

/// Compact the importer output in the same authored key order as import.ts.
pub(super) fn compact(value: &Value) -> Result<Value, ImportError> {
    let mut source = validate(value)?;
    let mut minimal = json!({"metadata": source["metadata"], "colors": {"primary": {}, "surface": {}, "interactive": {}, "status": {}, "syntax": {"base": source["colors"]["syntax"]["base"]}}});
    for (group, keys) in [
        ("primary", &["base"][..]),
        ("surface", &SURFACE[..5]),
        ("interactive", &["border"][..]),
        ("status", &["error", "warning", "success", "info"][..]),
    ] {
        for key in keys {
            minimal["colors"][group][key] = source["colors"][group][key].clone();
        }
    }
    let defaults = resolved_colors(&minimal);
    for (group, keep) in [
        ("primary", &["base"][..]),
        ("surface", &SURFACE[..5]),
        ("interactive", &["border"][..]),
        ("status", &["error", "warning", "success", "info"][..]),
    ] {
        if let Some(values) = source["colors"][group].as_object_mut() {
            values.retain(|key, value| {
                keep.contains(&key.as_str())
                    || !equal(value, defaults.get(&format!("{group}.{key}")))
            });
        }
    }
    for key in ["background", "foreground"] {
        if source["colors"]["syntax"]["base"][key]
            .as_str()
            .zip(source["colors"]["surface"][key].as_str())
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            source["colors"]["syntax"]["base"]
                .as_object_mut()
                .expect("syntax base")
                .remove(key);
        }
    }
    // Dependencies (enum -> className, methodCall -> method, etc.) must be
    // resolved afresh after each removal, matching JS insertion order.
    for key in [
        "className",
        "interface",
        "enum",
        "method",
        "variableProperty",
        "parameter",
        "constant",
        "boolean",
        "punctuation",
        "stringEscape",
        "commentDoc",
        "keywordImport",
        "tag",
        "tagAttribute",
    ] {
        let mut tokens = Colors::new();
        flatten(&source["colors"]["syntax"]["tokens"], "", &mut tokens);
        let Some(value) = tokens.remove(key) else {
            continue;
        };
        let inherited = syntax_tokens(&group(&defaults, "syntax.base."), &tokens);
        if inherited
            .get(key)
            .is_some_and(|other| value.eq_ignore_ascii_case(other))
        {
            source["colors"]["syntax"]["tokens"]
                .as_object_mut()
                .expect("syntax tokens")
                .remove(key);
        }
    }
    if source["colors"]["syntax"]["tokens"]
        .as_object()
        .is_some_and(Map::is_empty)
    {
        source["colors"]["syntax"]
            .as_object_mut()
            .expect("syntax colors")
            .remove("tokens");
    }
    let mut inherited = source.clone();
    if let Some(highlights) = inherited["colors"]["syntax"]["highlights"].as_object_mut() {
        highlights
            .retain(|key, _| ["diffAdded", "diffRemoved", "diffModified"].contains(&key.as_str()));
    }
    let inherited = resolved_colors(&inherited);
    if let Some(highlights) = source["colors"]["syntax"]["highlights"].as_object_mut() {
        highlights.retain(|key, value| {
            ["diffAdded", "diffRemoved", "diffModified"].contains(&key.as_str())
                || !equal(value, inherited.get(&format!("syntax.highlights.{key}")))
        });
    }
    let mut inherited = source.clone();
    inherited["colors"]
        .as_object_mut()
        .expect("colors")
        .remove("pr");
    let inherited = resolved_colors(&inherited);
    if let Some(pr) = source["colors"]["pr"].as_object_mut() {
        pr.retain(|key, value| !equal(value, inherited.get(&format!("pr.{key}"))));
    }
    Ok(source)
}

fn equal(value: &Value, other: Option<&String>) -> bool {
    value
        .as_str()
        .zip(other)
        .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
}
