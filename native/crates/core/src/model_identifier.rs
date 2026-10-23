//! Model references as OpenCode 2 stores them in config: `providerID/modelID`
//! with an optional `#variant` suffix, as `parseModelSelection` and
//! `formatModelSelection` in `packages/ui/src/lib/modelIdentifier.ts` split
//! and join them.

/// A provider's model with an optional variant.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    pub variant: Option<String>,
}

/// Parses `providerID/modelID[#variant]`. An embedded variant wins over
/// `variant`; an empty or malformed variant is dropped, not the model.
pub fn parse_model_selection(model: &str, variant: Option<&str>) -> Option<ModelSelection> {
    let trimmed = model.trim();
    let separator = trimmed.find('/').filter(|&index| index > 0)?;
    let provider_id = &trimmed[..separator];
    if provider_id.contains('#') {
        return None;
    }
    let rest = &trimmed[separator + 1..];
    let (model_id, embedded) = match rest.find('#') {
        Some(hash) => (&rest[..hash], Some(&rest[hash + 1..])),
        None => (rest, None),
    };
    if model_id.is_empty() {
        return None;
    }
    let chosen = embedded.or_else(|| variant.map(str::trim));
    let variant = chosen
        .filter(|chosen| !chosen.is_empty() && !chosen.contains('#'))
        .map(str::to_owned);
    Some(ModelSelection {
        provider_id: provider_id.to_owned(),
        model_id: model_id.to_owned(),
        variant,
    })
}

/// Joins a selection as `providerID/modelID[#variant]`, or `None` when the
/// provider or model is blank.
pub fn format_model_selection(selection: &ModelSelection) -> Option<String> {
    let provider_id = selection.provider_id.trim();
    let model_id = selection.model_id.trim();
    if provider_id.is_empty() || model_id.is_empty() {
        return None;
    }
    Some(match selection.variant.as_deref().map(str::trim) {
        Some(variant) if !variant.is_empty() => format!("{provider_id}/{model_id}#{variant}"),
        _ => format!("{provider_id}/{model_id}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(provider: &str, model: &str, variant: Option<&str>) -> ModelSelection {
        ModelSelection {
            provider_id: provider.to_owned(),
            model_id: model.to_owned(),
            variant: variant.map(str::to_owned),
        }
    }

    #[test]
    fn parses_as_the_web_client_does() {
        assert_eq!(
            parse_model_selection(" perf/stream-300cps ", None),
            Some(selection("perf", "stream-300cps", None))
        );
        assert_eq!(
            parse_model_selection("perf/stream-300cps#high", Some("low")),
            Some(selection("perf", "stream-300cps", Some("high"))),
            "an embedded variant wins"
        );
        assert_eq!(
            parse_model_selection("perf/stream-300cps", Some(" low ")),
            Some(selection("perf", "stream-300cps", Some("low")))
        );
        assert_eq!(
            parse_model_selection("perf/stream-300cps#", Some("low")),
            Some(selection("perf", "stream-300cps", None)),
            "an empty embedded variant drops the variant"
        );
        assert_eq!(
            parse_model_selection("perf/a#b#c", None),
            Some(selection("perf", "a", None)),
            "a variant holding a hash is dropped"
        );
        assert_eq!(
            parse_model_selection("openrouter/meta/llama", None),
            Some(selection("openrouter", "meta/llama", None)),
            "model IDs may hold slashes"
        );
        assert_eq!(parse_model_selection("/model", None), None);
        assert_eq!(parse_model_selection("perf/", None), None);
        assert_eq!(parse_model_selection("pe#rf/model", None), None);
        assert_eq!(parse_model_selection("   ", None), None);
    }

    #[test]
    fn formats_with_the_variant_suffix() {
        assert_eq!(
            format_model_selection(&selection(" perf ", "stream", Some(" high "))).as_deref(),
            Some("perf/stream#high")
        );
        assert_eq!(
            format_model_selection(&selection("perf", "stream", Some(" "))).as_deref(),
            Some("perf/stream")
        );
        assert_eq!(format_model_selection(&selection("", "stream", None)), None);
    }
}
