use serde_json::Value;

use super::{
    ImportError, MAX_THEME_IMPORT_BYTES, ResolvedTheme, import_vscode_theme, resolve_theme,
};

fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures.json")).expect("canonical fixtures")
}

fn assert_resolved(actual: ResolvedTheme, expected: &Value, label: &str) {
    assert_eq!(actual.metadata, expected["metadata"], "{label}: metadata");
    let expected_colors = serde_json::from_value(expected["colors"].clone()).expect("colors");
    assert_eq!(actual.colors, expected_colors, "{label}: colors");
    let actual_palette: Vec<_> = actual
        .agent_palette
        .iter()
        .map(|role| serde_json::json!({"index":role.index,"colorful":role.colorful}))
        .collect();
    assert_eq!(
        serde_json::json!(actual_palette),
        expected["agentPalette"],
        "{label}: agent palette"
    );
}

#[test]
fn imports_match_current_canonical_typescript() {
    for (index, case) in fixtures()["imports"]
        .as_array()
        .expect("imports")
        .iter()
        .enumerate()
    {
        let label = format!("import {index}: {}", case["filename"]);
        let result = import_vscode_theme(
            case["text"].as_str().expect("text"),
            case["filename"].as_str().expect("filename"),
        );
        if let Some(error) = case["error"].as_str() {
            assert_eq!(result.expect_err(&label).code(), error, "{label}");
        } else {
            let definition = result.unwrap_or_else(|error| panic!("{label}: {error}"));
            assert_eq!(
                definition, case["definition"],
                "{label}: compact server definition"
            );
            assert_resolved(
                resolve_theme(&definition).expect("valid imported theme"),
                &case["resolved"],
                &label,
            );
        }
    }
}

#[test]
fn definitions_and_every_builtin_match_canonical_typescript() {
    for (index, case) in fixtures()["definitions"]
        .as_array()
        .expect("definitions")
        .iter()
        .enumerate()
    {
        let label = format!(
            "definition {index}: {}",
            case["definition"]["metadata"]["id"]
        );
        let result = resolve_theme(&case["definition"]);
        if case.get("error").is_some() {
            assert_eq!(result.expect_err(&label), ImportError::Invalid, "{label}");
        } else {
            assert_resolved(
                result.unwrap_or_else(|error| panic!("{label}: {error}")),
                &case["resolved"],
                &label,
            );
        }
    }
}

#[test]
fn file_limit_counts_utf8_bytes_before_parsing() {
    assert_eq!(super::css_opacity("#ffffff22", 0.5), "#ffffff80");
    assert_eq!(
        super::catalog_base_name("Night Owl (No Italics)"),
        "night owl"
    );
    assert_eq!(
        import_vscode_theme(&" ".repeat(MAX_THEME_IMPORT_BYTES + 1), "large.json"),
        Err(ImportError::Size)
    );
    assert_eq!(
        import_vscode_theme(&"é".repeat(MAX_THEME_IMPORT_BYTES / 2 + 1), "large.json"),
        Err(ImportError::Size)
    );
    assert_eq!(
        import_vscode_theme(&" ".repeat(MAX_THEME_IMPORT_BYTES), "large.json"),
        Err(ImportError::Invalid)
    );
}

#[test]
fn css_paint_matches_chrome_over_opaque_backgrounds() {
    let fixtures: Value =
        serde_json::from_str(include_str!("paint-fixtures.json")).expect("Chrome fixtures");
    for case in fixtures["cases"].as_array().expect("cases") {
        let value = case["value"].as_str().expect("value");
        let dark = case["dark"].as_bool().expect("dark");
        let result = super::resolve_css_color(
            value,
            dark,
            Some(super::css_color_context("#234567", dark).expect("inherited foreground")),
        );
        if case["unsupported"] == true {
            assert!(
                matches!(result, Err(super::PaintError::Unsupported(_))),
                "{value}: unsupported semantics must stay explicit"
            );
            continue;
        }
        if case["invalid"] == true {
            assert_eq!(case["valid"], false, "reference accepted {value}");
            assert_eq!(result, Err(super::PaintError::Invalid), "{value}");
            continue;
        }
        assert_eq!(case["valid"], true, "reference rejected {value}");
        let color = result.unwrap_or_else(|error| panic!("{value}: {error}"));
        for (background, samples) in case["samples"]
            .as_array()
            .expect("samples")
            .iter()
            .enumerate()
        {
            for channel in 0..3 {
                let painted =
                    (color[channel] * color[3] + background as f32 * (1.0 - color[3])) * 255.0;
                let expected = samples[channel].as_f64().expect("channel") as f32;
                assert!(
                    (painted.round() - expected).abs() <= 1.0,
                    "{value}, dark={dark}, background={background}, channel={channel}: native={painted}, Chrome={expected}"
                );
            }
        }
    }
}

#[test]
fn modern_hsl_hwb_accept_numeric_components_but_legacy_requires_percentages() {
    for (value, expected) in [
        ("hsl(120 100 50)", [0.0, 1.0, 0.0, 1.0]),
        ("hsl(120 100% 50)", [0.0, 1.0, 0.0, 1.0]),
        ("hsla(120 100 50% / 0.4)", [0.0, 1.0, 0.0, 0.4]),
        ("hwb(120 30 50)", [0.3, 0.5, 0.3, 1.0]),
        ("hwb(120 30% 50 / 50%)", [0.3, 0.5, 0.3, 0.5]),
    ] {
        assert_eq!(
            super::resolve_css_color(value, false, None),
            Ok(expected),
            "{value}"
        );
    }
    for value in [
        "hsl(120, 100, 50)",
        "hsl(120, 100%, 50)",
        "hsla(120, 100, 50%, 0.5)",
        "hwb(120, 30%, 50%)",
    ] {
        assert_eq!(
            super::resolve_css_color(value, false, None),
            Err(super::PaintError::Invalid),
            "{value}"
        );
    }
}

#[test]
fn color_mix_accepts_one_leading_or_trailing_weight_per_color() {
    for (leading, trailing) in [
        (
            "color-mix(in srgb, 70% red, 50% blue)",
            "color-mix(in srgb, red 70%, blue 50%)",
        ),
        (
            "color-mix(in srgb, 25% #ff000080, #0000ff40 25%)",
            "color-mix(in srgb, #ff000080 25%, #0000ff40 25%)",
        ),
        (
            "color-mix(in srgb, 20% rgb(255 0 0 / 0.5), blue)",
            "color-mix(in srgb, rgb(255 0 0 / 0.5) 20%, blue)",
        ),
    ] {
        let expected = super::resolve_css_color(trailing, false, None).expect("trailing weight");
        assert_eq!(
            super::resolve_css_color(leading, false, None),
            Ok(expected),
            "{leading}"
        );
    }
    for value in [
        "color-mix(in srgb, 20% red 30%, blue)",
        "color-mix(in srgb, 101% red, blue)",
        "color-mix(in srgb, -1% red, blue)",
    ] {
        assert_eq!(
            super::resolve_css_color(value, false, None),
            Err(super::PaintError::Invalid),
            "{value}"
        );
    }
}

#[test]
fn hwb_normalizes_unclipped_nonnegative_components() {
    for value in ["hwb(0 200% 100%)", "hwb(0 200 100)"] {
        assert_eq!(
            super::resolve_css_color(value, false, None),
            Ok([2.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0, 1.0]),
            "{value}"
        );
    }
    assert_eq!(
        super::resolve_css_color("hwb(0 -20 50)", false, None),
        Ok([0.5, 0.0, 0.0, 1.0])
    );
    assert_eq!(
        super::resolve_css_color("hsl(120 200 50)", false, None),
        Ok([0.0, 1.0, 0.0, 1.0])
    );
}

#[test]
fn css_numbers_require_digits_after_decimal_points_and_exponents() {
    let invalid = [
        "hsl(120 100. 50)",
        "color-mix(in srgb, 20.% red, blue)",
        "rgb(1.e2 0 0)",
        "hsl(120 . 50)",
        "rgb(1e+ 0 0)",
        "rgb(++1 0 0)",
        "rgb(NaN 0 0)",
        "rgb(inf 0 0)",
        "rgb(1e999 0 0)",
    ];
    let accepted: Vec<_> = invalid
        .into_iter()
        .filter(|value| {
            super::resolve_css_color(value, false, None) != Err(super::PaintError::Invalid)
        })
        .collect();
    assert!(
        accepted.is_empty(),
        "invalid numeric tokens accepted: {accepted:?}"
    );
    for (value, equivalent) in [
        ("hsl(+1.2e2 +1E+2 5e1 / +.5)", "hsl(120 100 50 / 0.5)"),
        ("hsl(120 100.0 50.0)", "hsl(120 100 50)"),
        ("rgb(+2.55e2 -0 .5e2 / 5E-1)", "rgb(255 0 50 / 0.5)"),
        (
            "color-mix(in srgb, +2.0e1% red, blue)",
            "color-mix(in srgb, 20% red, blue)",
        ),
    ] {
        let expected = super::resolve_css_color(equivalent, false, None).expect("valid color");
        assert_eq!(
            super::resolve_css_color(value, false, None),
            Ok(expected),
            "{value}"
        );
    }
}

#[test]
fn missing_alpha_paints_as_zero_and_carries_the_other_mix_alpha() {
    assert_eq!(
        super::resolve_css_color("rgb(200 100 50 / none)", false, None),
        Ok([200.0 / 255.0, 100.0 / 255.0, 50.0 / 255.0, 0.0])
    );
    for value in [
        "color-mix(in srgb, rgb(200 100 50 / none), rgb(100 200 100 / 0.4))",
        "color-mix(in srgb, rgb(200 100 50 / 0.4), rgb(100 200 100 / none))",
    ] {
        assert_eq!(
            super::resolve_css_color(value, false, None),
            Ok([150.0 / 255.0, 150.0 / 255.0, 75.0 / 255.0, 0.4]),
            "{value}"
        );
    }
}

#[test]
fn nested_mix_preserves_color_when_both_alphas_are_missing() {
    let value = "color-mix(in srgb, color-mix(in srgb, rgb(200 100 50 / none), rgb(100 200 100 / none)), red)";
    assert_eq!(
        super::resolve_css_color(value, false, None),
        Ok([202.5 / 255.0, 75.0 / 255.0, 37.5 / 255.0, 1.0])
    );
}

#[test]
fn current_color_preserves_missing_alpha_until_token_painting() {
    let token = "color-mix(in srgb, currentColor, rgb(100 200 100 / 0.4))";
    for dark in [false, true] {
        for (foreground, expected) in [
            (
                "rgb(200 100 50 / none)",
                [150.0 / 255.0, 150.0 / 255.0, 75.0 / 255.0, 0.4],
            ),
            (
                "rgb(200 100 50 / 0)",
                [100.0 / 255.0, 200.0 / 255.0, 100.0 / 255.0, 0.2],
            ),
        ] {
            let current = super::css_color_context(foreground, dark).expect("foreground");
            assert_eq!(
                super::resolve_css_color(token, dark, Some(current)),
                Ok(expected),
                "{foreground}, dark={dark}"
            );
        }
    }
}

#[test]
fn current_color_matches_chrome_consumer_styles_and_inheritance() {
    let fixtures: Value = serde_json::from_str(include_str!("currentcolor-fixtures.json"))
        .expect("Chrome consumer references");
    for case in fixtures["cases"].as_array().expect("cases") {
        let dark = case["dark"].as_bool().expect("dark");
        let current = case["foreground"]
            .as_str()
            .map(|value| super::css_color_context(value, dark).expect("foreground"));
        let color = super::resolve_css_color(case["token"].as_str().expect("token"), dark, current)
            .expect("consumer token");
        for (background, samples) in case["samples"]
            .as_array()
            .expect("samples")
            .iter()
            .enumerate()
        {
            for channel in 0..3 {
                let painted =
                    (color[channel] * color[3] + background as f32 * (1.0 - color[3])) * 255.0;
                let expected = samples[channel].as_f64().expect("channel") as f32;
                assert!(
                    (painted.round() - expected).abs() <= 1.0,
                    "{case}: background={background}, channel={channel}: native={painted}, Chrome={expected}"
                );
            }
        }
    }
}
