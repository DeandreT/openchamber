//! Default send configuration and composer labels, with explicit catalog and routing inputs.

use std::collections::{HashMap, HashSet};

/// A model as the catalog lists it, under its provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogModel {
    /// The catalog key, including a derived model's suffix.
    pub id: String,
    /// The provider API name when it differs from the catalog key.
    pub provider_model_id: Option<String>,
    /// The catalog's display name.
    pub name: Option<String>,
    /// Supported variant identifiers.
    pub variants: Vec<String>,
}

/// A provider with its models, disabled providers and models already removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogProvider {
    /// The provider identifier.
    pub id: String,
    /// The provider's display name.
    pub name: Option<String>,
    /// Models in catalog order.
    pub models: Vec<CatalogModel>,
}

/// A model reference with an optional variant.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModelChoice {
    /// The provider identifier.
    pub provider_id: String,
    /// The selected catalog key or provider API name.
    pub model_id: String,
    /// The selected variant, absent for the default effort.
    pub variant: Option<String>,
}

/// Which conversation roles an agent supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentMode {
    /// A top-level conversation agent.
    Primary,
    /// An agent available only through delegation.
    Subagent,
    /// An agent available in both roles.
    All,
}

/// An agent as the catalog lists it. `name` is the agent ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEntry {
    /// The machine identifier, projected from OpenCode's agent ID.
    pub name: String,
    /// The optional display name.
    pub display_name: Option<String>,
    /// An absent mode supports primary conversations.
    pub mode: Option<AgentMode>,
    /// The logical OR of the catalog's top-level and options-level hidden flags.
    pub hidden: bool,
    /// The agent's pinned model and variant.
    pub model: Option<ModelChoice>,
    /// The configured color, validated by the color resolver.
    pub color: Option<String>,
}

/// Whether `openchamber/auto` can be honoured: unknown readiness keeps it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AutoRouting {
    /// Settings or routing readiness have not loaded.
    #[default]
    Unknown,
    /// The server does not expose the routing feature.
    Unavailable,
    /// The server can route Auto choices.
    Ready,
    /// The loaded server routing state cannot route Auto choices.
    NotReady,
}

/// Every input of the default selection besides the catalogs.
///
/// Settings and project fields are already normalized by their settings adapters.
/// Model identifier parsing itself preserves whitespace, as the web parser does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionDefaults {
    /// The project's default agent.
    pub project_default_agent: Option<String>,
    /// The project's default model identifier.
    pub project_default_model: Option<String>,
    /// The project's default variant.
    pub project_default_variant: Option<String>,
    /// The global settings default agent.
    pub settings_default_agent: Option<String>,
    /// The global settings default model identifier.
    pub settings_default_model: Option<String>,
    /// The global settings default variant.
    pub settings_default_variant: Option<String>,
    /// OpenCode's default agent.
    pub opencode_default_agent: Option<String>,
    /// OpenCode's normalized `provider/model#variant` config value.
    pub opencode_default_model: Option<String>,
    /// The last composer model identifier.
    pub last_selected_model: Option<String>,
    /// Provider and model identifier pairs hidden in the picker.
    pub hidden_models: Vec<(String, String)>,
    /// False until settings load: stops before the agent's and OpenCode's models.
    pub allow_fallback: bool,
    /// Readiness after the settings-loaded guard has been applied.
    pub auto_routing: AutoRouting,
}

/// The default agent and model, either of which may still be unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// The selected agent identifier.
    pub agent_name: Option<String>,
    /// The selected model and validated effort.
    pub model: Option<ModelChoice>,
}

/// A flat catalog model before disabled entries have been removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlatModel {
    /// The catalog key.
    pub id: String,
    /// The owning provider.
    pub provider_id: String,
    /// The provider API name when it differs from the catalog key.
    pub provider_model_id: Option<String>,
    /// The display name.
    pub name: Option<String>,
    /// Supported variant identifiers.
    pub variants: Vec<String>,
    /// OpenCode's model-enabled flag.
    pub enabled: bool,
}

/// A flat provider before disabled or model-less providers have been removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlatProvider {
    /// The provider identifier.
    pub id: String,
    /// The display name.
    pub name: Option<String>,
    /// True whenever the provider activation is anything except `disabled`.
    pub enabled: bool,
}

/// Regroups enabled models under enabled providers in the original catalog order.
#[must_use]
pub fn group_catalog(providers: &[FlatProvider], models: &[FlatModel]) -> Vec<CatalogProvider> {
    let mut grouped: HashMap<&str, Vec<CatalogModel>> = HashMap::new();
    for model in models.iter().filter(|model| model.enabled) {
        grouped
            .entry(&model.provider_id)
            .or_default()
            .push(CatalogModel {
                id: model.id.clone(),
                provider_model_id: model.provider_model_id.clone(),
                name: model.name.clone(),
                variants: model
                    .variants
                    .iter()
                    .filter(|id| !id.is_empty())
                    .cloned()
                    .collect(),
            });
    }
    providers
        .iter()
        .filter(|provider| provider.enabled)
        .filter_map(|provider| {
            Some(CatalogProvider {
                id: provider.id.clone(),
                name: provider.name.clone(),
                models: grouped.get(provider.id.as_str())?.clone(),
            })
        })
        .collect()
}

fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.is_empty())
}

fn parse_identifier(value: &str) -> Option<ModelChoice> {
    let (provider, model) = value.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some(ModelChoice {
        provider_id: provider.to_owned(),
        model_id: model.to_owned(),
        variant: None,
    })
}

fn parse_selection(value: &str) -> Option<ModelChoice> {
    let mut choice = parse_identifier(js_trim(value))?;
    if choice.provider_id.contains('#') {
        return None;
    }
    if let Some((model, variant)) = choice.model_id.split_once('#') {
        if model.is_empty() {
            return None;
        }
        choice.variant =
            (!variant.is_empty() && !variant.contains('#')).then(|| variant.to_owned());
        choice.model_id = model.to_owned();
    }
    Some(choice)
}

fn is_auto(choice: &ModelChoice) -> bool {
    choice.provider_id == "openchamber" && choice.model_id == "auto"
}

fn stale_auto(choice: &ModelChoice, routing: AutoRouting) -> bool {
    is_auto(choice) && matches!(routing, AutoRouting::Unavailable | AutoRouting::NotReady)
}

/// The catalog entry `choice` names: by catalog key, else by provider API name.
pub fn find_model<'a>(
    providers: &'a [CatalogProvider],
    choice: &ModelChoice,
) -> Option<&'a CatalogModel> {
    if choice.model_id.is_empty() {
        return None;
    }
    let models = &providers
        .iter()
        .find(|provider| provider.id == choice.provider_id)?
        .models;
    models
        .iter()
        .find(|model| model.id == choice.model_id)
        .or_else(|| {
            models.iter().find(|model| {
                model.provider_model_id.as_deref().unwrap_or(&model.id) == choice.model_id
            })
        })
}

fn validate_variant(
    mut choice: ModelChoice,
    providers: &[CatalogProvider],
    routing: AutoRouting,
) -> ModelChoice {
    let model = find_model(providers, &choice);
    let exists = if is_auto(&choice) {
        routing == AutoRouting::Ready
    } else {
        model.is_some()
    };
    if exists
        && !choice.variant.as_ref().is_some_and(|variant| {
            !variant.is_empty() && model.is_some_and(|model| model.variants.contains(variant))
        })
    {
        choice.variant = None;
    }
    choice
}

fn hidden(defaults: &SelectionDefaults, provider: &str, model: &str) -> bool {
    defaults
        .hidden_models
        .iter()
        .any(|(p, m)| p == provider && m == model)
}

fn primary(agent: &AgentEntry) -> bool {
    agent.mode != Some(AgentMode::Subagent) && !agent.hidden
}

fn fallback_model(
    providers: &[CatalogProvider],
    defaults: &SelectionDefaults,
) -> Option<ModelChoice> {
    if let Some(choice) = nonempty(&defaults.last_selected_model).and_then(parse_identifier)
        && !hidden(defaults, &choice.provider_id, &choice.model_id)
        && !stale_auto(&choice, defaults.auto_routing)
    {
        return Some(choice);
    }
    let pickle = ModelChoice {
        provider_id: "opencode".to_owned(),
        model_id: "big-pickle".to_owned(),
        variant: None,
    };
    let has_pickle = find_model(providers, &pickle).is_some();
    if has_pickle && !hidden(defaults, &pickle.provider_id, &pickle.model_id) {
        return Some(pickle);
    }
    for provider in providers {
        for model in &provider.models {
            let id = model.provider_model_id.as_deref().unwrap_or(&model.id);
            if !hidden(defaults, &provider.id, id) {
                return Some(ModelChoice {
                    provider_id: provider.id.clone(),
                    model_id: id.to_owned(),
                    variant: None,
                });
            }
        }
    }
    if has_pickle {
        return Some(pickle);
    }
    let provider = providers.first()?;
    let model = provider.models.first()?;
    Some(ModelChoice {
        provider_id: provider.id.clone(),
        model_id: model
            .provider_model_id
            .as_ref()
            .unwrap_or(&model.id)
            .clone(),
        variant: None,
    })
}

/// Resolves the same default cascade as the web composer, preserving catalog gaps.
#[must_use]
pub fn resolve_default_selection(
    agents: &[AgentEntry],
    providers: &[CatalogProvider],
    defaults: &SelectionDefaults,
) -> Selection {
    let named = |name: &str| agents.iter().find(|agent| agent.name == name);
    let agent = nonempty(&defaults.project_default_agent)
        .and_then(named)
        .or_else(|| nonempty(&defaults.settings_default_agent).and_then(named))
        .or_else(|| {
            nonempty(&defaults.opencode_default_agent)
                .and_then(named)
                .filter(|agent| primary(agent))
        })
        .or_else(|| {
            agents
                .iter()
                .find(|agent| agent.name == "build" && primary(agent))
        })
        .or_else(|| agents.iter().find(|agent| primary(agent)))
        .or_else(|| agents.first());
    let configured = nonempty(&defaults.project_default_model)
        .or_else(|| nonempty(&defaults.settings_default_model));
    let model = configured
        .and_then(parse_identifier)
        .filter(|choice| !stale_auto(choice, defaults.auto_routing))
        .map(|mut choice| {
            choice.variant = if nonempty(&defaults.project_default_model).is_some() {
                defaults.project_default_variant.clone()
            } else {
                defaults.settings_default_variant.clone()
            };
            validate_variant(choice, providers, defaults.auto_routing)
        });
    if model.is_some() || !defaults.allow_fallback {
        return Selection {
            agent_name: agent
                .map(|agent| agent.name.clone())
                .or_else(|| defaults.project_default_agent.clone())
                .or_else(|| defaults.settings_default_agent.clone()),
            model,
        };
    }
    let model = agent
        .and_then(|agent| agent.model.clone())
        .filter(|choice| !choice.provider_id.is_empty() && !choice.model_id.is_empty())
        .or_else(|| nonempty(&defaults.opencode_default_model).and_then(parse_selection))
        .map(|choice| validate_variant(choice, providers, defaults.auto_routing))
        .or_else(|| fallback_model(providers, defaults));
    Selection {
        agent_name: agent.map(|agent| agent.name.clone()),
        model,
    }
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.len_utf16() == 2 => value.to_owned(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.len_utf16() == 2 => first.to_string() + &chars.as_str().to_lowercase(),
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn decimal(value: &str) -> bool {
    value.split_once('.').map_or_else(
        || digits(value),
        |(whole, fraction)| digits(whole) && digits(fraction),
    )
}

fn format_token(token: &str) -> String {
    let lower = token.to_lowercase();
    let mapped = match lower.as_str() {
        "ai" => "AI",
        "api" => "API",
        "ernie" => "ERNIE",
        "glm" => "GLM",
        "gpt" => "GPT",
        "it" => "IT",
        "lfm" => "LFM",
        "lm" => "LM",
        "oss" => "OSS",
        "pdf" => "PDF",
        "rp" => "RP",
        "slerp" => "SLERP",
        "tars" => "TARS",
        "ui" => "UI",
        "vl" => "VL",
        "vlm" => "VLM",
        "aion" => "Aion",
        "anthropic" => "Anthropic",
        "chatgpt" => "ChatGPT",
        "claude" => "Claude",
        "codex" => "Codex",
        "codestral" => "Codestral",
        "command" => "Command",
        "deepseek" => "DeepSeek",
        "gemini" => "Gemini",
        "gemma" => "Gemma",
        "grok" => "Grok",
        "hunyuan" => "Hunyuan",
        "kimi" => "Kimi",
        "llama" => "Llama",
        "minimax" => "MiniMax",
        "mistral" => "Mistral",
        "mixtral" => "Mixtral",
        "nemotron" => "Nemotron",
        "openai" => "OpenAI",
        "qwen" => "Qwen",
        "rekaai" => "RekaAI",
        _ => "",
    };
    if !mapped.is_empty() {
        return mapped.to_owned();
    }
    if let Some(version) = lower.strip_prefix("qwen")
        && decimal(version)
        && version
            .split('.')
            .next()
            .is_some_and(|part| part.len() == 1)
    {
        return format!("Qwen{version}");
    }
    let bytes = lower.as_bytes();
    if bytes.len() >= 2 && bytes[1].is_ascii_digit() {
        if bytes[0] == b'o' {
            return lower;
        }
        if bytes[0] == b'v' {
            return format!(
                "V{}",
                token.chars().skip(1).collect::<String>().to_uppercase()
            );
        }
    }
    if let Some(number) = lower.strip_suffix(['o', 'n'])
        && digits(number)
    {
        return lower;
    }
    if bytes.first().is_some_and(u8::is_ascii_lowercase) {
        let remainder = &lower[1..];
        let number = remainder
            .strip_suffix(|c: char| c.is_ascii_lowercase())
            .unwrap_or(remainder);
        if digits(number) {
            return lower.to_uppercase();
        }
    }
    let tail = lower.trim_end_matches(|c: char| c.is_ascii_lowercase());
    let unit = &lower[tail.len()..];
    let integer_or_product = tail
        .split_once('x')
        .map_or_else(|| digits(tail), |(a, b)| digits(a) && digits(b));
    if (!unit.is_empty() && integer_or_product) || (unit.len() == 1 && decimal(tail)) {
        return format!("{tail}{}", unit.to_uppercase());
    }
    title_case(token)
}

fn is_date(value: &str) -> bool {
    value.len() == 10
        && (value.starts_with("19") || value.starts_with("20"))
        && value.split('-').map(str::len).eq([4, 2, 2])
        && value.split('-').all(digits)
}

fn humanize_model_id(value: &str) -> String {
    let value = js_trim(value);
    let value = value.strip_prefix('~').unwrap_or(value);
    let value = value.rsplit('/').next().unwrap_or(value);
    let (base, suffix) = value
        .rsplit_once(':')
        .filter(|(base, suffix)| !base.is_empty() && !suffix.is_empty())
        .unwrap_or((value, ""));
    let mut separated = String::new();
    let mut previous = None;
    for ch in base.chars() {
        if ch.is_ascii_uppercase()
            && previous.is_some_and(|prev: char| prev.is_ascii_lowercase() || prev.is_ascii_digit())
        {
            separated.push('-');
        }
        separated.push(ch);
        previous = Some(ch);
    }
    let raw: Vec<&str> = separated
        .split(|ch: char| ch == '-' || ch == '_' || is_js_whitespace(ch))
        .filter(|token| !token.is_empty())
        .collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        if index + 2 < raw.len()
            && raw[index].len() == 4
            && (raw[index].starts_with("19") || raw[index].starts_with("20"))
            && digits(raw[index])
            && raw[index + 1].len() == 2
            && digits(raw[index + 1])
            && raw[index + 2].len() == 2
            && digits(raw[index + 2])
        {
            tokens.push(format!(
                "{}-{}-{}",
                raw[index],
                raw[index + 1],
                raw[index + 2]
            ));
            index += 3;
        } else if index + 1 < raw.len()
            && raw[index].len() == 1
            && digits(raw[index])
            && raw[index + 1].len() == 1
            && digits(raw[index + 1])
        {
            tokens.push(format!("{}.{}", raw[index], raw[index + 1]));
            index += 2;
        } else {
            tokens.push(raw[index].to_owned());
            index += 1;
        }
    }
    let mut tokens: Vec<String> = tokens.iter().map(|token| format_token(token)).collect();
    if tokens.len() >= 2 {
        let number = tokens[1].starts_with(|ch: char| ch.is_ascii_digit());
        let joiner = match tokens[0].as_str() {
            "GPT" if number || tokens[1] == "OSS" => Some("-"),
            "GLM" if number => Some("-"),
            "LFM" | "Qwen" if number => Some(""),
            _ => None,
        };
        if let Some(joiner) = joiner {
            let second = tokens.remove(1);
            tokens[0].push_str(joiner);
            tokens[0].push_str(&second);
        }
        if tokens.len() >= 2
            && tokens[tokens.len() - 2] == "Non"
            && tokens.last().is_some_and(|token| token == "Reasoning")
        {
            tokens.truncate(tokens.len() - 2);
            tokens.push("(Non-Reasoning)".to_owned());
        }
    }
    if let Some(last) = tokens.last_mut()
        && is_date(last)
    {
        *last = format!("({last})");
    }
    let name = tokens.join(" ");
    let suffix = js_trim(suffix).to_lowercase();
    if suffix.is_empty() {
        return name;
    }
    let suffix = if suffix == "free" || suffix == "thinking" {
        suffix
    } else {
        humanize_model_id(&suffix)
    };
    if suffix.is_empty() {
        name
    } else {
        format!("{name} ({suffix})")
    }
}

/// The toolbar's catalog name or humanized model ID, cut to 40 UTF-16 code units.
/// A cut through a surrogate pair uses the Unicode replacement character in Rust.
#[must_use]
pub fn model_label(providers: &[CatalogProvider], choice: &ModelChoice) -> String {
    // Display lookup trims catalog keys; selection lookup preserves their spelling.
    let id = js_trim(&choice.model_id);
    let model = providers
        .iter()
        .find(|provider| provider.id == choice.provider_id)
        .filter(|_| !id.is_empty())
        .and_then(|provider| {
            provider
                .models
                .iter()
                .find(|model| js_trim(&model.id) == id)
                .or_else(|| {
                    provider.models.iter().find(|model| {
                        js_trim(model.provider_model_id.as_deref().unwrap_or(&model.id)) == id
                    })
                })
        });
    let name = model
        .and_then(|model| model.name.as_deref())
        .map(js_trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let id = model
                .map(|model| js_trim(&model.id))
                .filter(|id| !id.is_empty())
                .unwrap_or(id);
            humanize_model_id(id)
        });
    if name.encode_utf16().count() <= 40 {
        return name;
    }
    let prefix: Vec<u16> = name.encode_utf16().take(37).collect();
    String::from_utf16_lossy(&prefix) + "..."
}

/// The toolbar's trimmed display name, otherwise its capitalized agent ID.
#[must_use]
pub fn agent_label(agent: &AgentEntry) -> String {
    agent
        .display_name
        .as_deref()
        .map(js_trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| agent_name_label(&agent.name))
}

/// Capitalizes an agent ID when no catalog entry is known.
#[must_use]
pub fn agent_name_label(name: &str) -> String {
    capitalize(name)
}

/// Which color the agent label takes; the desktop maps it to theme tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentColor {
    /// A validated six-digit configured hex color.
    Configured(String),
    /// The theme's status success token.
    Success,
    /// A syntax role index and the number of roles in the theme's source list.
    Syntax { index: usize, of: usize },
}

/// One role in the theme's ordered, contrast-filtered agent palette.
///
/// The theme adapter prepares this list as `agentColorPalette` does: keyword,
/// type, function, number, string, operator, variable are the source indices.
/// Separation and visibility determine ordering and inclusion; `colorful` is
/// chromatic distance from neutral grey of at least 0.035 on each background.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SyntaxPaletteEntry {
    /// The index in the theme's original syntax role list.
    pub index: usize,
    /// Whether allocation should prefer this role over neutral alternatives.
    pub colorful: bool,
}

fn hash_name(name: &str) -> usize {
    let mut hash = 0_u32;
    for unit in name.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(u32::from(unit));
    }
    hash as usize
}

fn configured_color(color: &str) -> Option<String> {
    let color = js_trim(color);
    (color.len() == 7
        && color.starts_with('#')
        && color.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
        && !color.eq_ignore_ascii_case("#aaaaaa"))
    .then(|| color.to_owned())
}

/// Resolves colors using every supplied syntax role in order as a colorful palette.
/// Use `agent_color_with_palette` for parity with a theme's filtered palette.
/// A theme with no syntax roles falls back to success.
#[must_use]
pub fn agent_color(name: Option<&str>, agents: &[AgentEntry], syntax_roles: usize) -> AgentColor {
    let palette: Vec<SyntaxPaletteEntry> = (0..syntax_roles)
        .map(|index| SyntaxPaletteEntry {
            index,
            colorful: true,
        })
        .collect();
    agent_color_with_palette(name, agents, &palette, syntax_roles)
}

/// Resolves configured colors and the web client's visible-roster palette allocation.
///
/// `palette` is the theme adapter's prepared palette. Invalid role indices are
/// ignored; an empty palette uses success. Configured colors override Build too,
/// while hidden agents neither reserve colors nor supply configured overrides.
#[must_use]
pub fn agent_color_with_palette(
    name: Option<&str>,
    agents: &[AgentEntry],
    palette: &[SyntaxPaletteEntry],
    syntax_roles: usize,
) -> AgentColor {
    let name = name.filter(|name| !name.is_empty());
    if let Some(color) = name.and_then(|name| {
        agents
            .iter()
            .filter(|agent| !agent.hidden && agent.name == name)
            .filter_map(|agent| agent.color.as_deref().and_then(configured_color))
            .next_back()
    }) {
        return AgentColor::Configured(color);
    }
    if name.is_none() || name == Some("build") {
        return AgentColor::Success;
    }
    let palette: Vec<&SyntaxPaletteEntry> = palette
        .iter()
        .filter(|entry| entry.index < syntax_roles)
        .collect();
    if palette.is_empty() {
        return AgentColor::Success;
    }
    let name = name.expect("the missing-name case returned success");
    let mut ordered: Vec<&AgentEntry> = agents.iter().filter(|agent| !agent.hidden).collect();
    ordered.sort_by(|a, b| {
        (a.mode == Some(AgentMode::Subagent))
            .cmp(&(b.mode == Some(AgentMode::Subagent)))
            .then_with(|| a.name.encode_utf16().cmp(b.name.encode_utf16()))
    });
    let mut assigned = HashSet::new();
    let mut used = HashSet::new();
    for agent in ordered {
        if agent.name == "build" || !assigned.insert(&agent.name) {
            continue;
        }
        if used.len() == palette.len() {
            used.clear();
        }
        let available: Vec<usize> = (0..palette.len())
            .filter(|index| !used.contains(index))
            .collect();
        let colorful: Vec<usize> = available
            .iter()
            .copied()
            .filter(|index| palette[*index].colorful)
            .collect();
        let choices = if colorful.is_empty() {
            &available
        } else {
            &colorful
        };
        let index = choices[hash_name(&agent.name) % choices.len()];
        if agent.name == name {
            return AgentColor::Syntax {
                index: palette[index].index,
                of: syntax_roles,
            };
        }
        used.insert(index);
    }
    AgentColor::Syntax {
        index: palette[hash_name(name) % palette.len()].index,
        of: syntax_roles,
    }
}
