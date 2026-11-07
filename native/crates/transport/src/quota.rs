//! Provider quotas (`lib/quota`): `GET /api/quota/:providerId` answers one
//! provider's usage windows, as `fetchQuota.ts` validates them.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::client::ServerClient;
use crate::error::TransportError;

/// `QUOTA_PROVIDERS`: every provider the Usage page lists, in its order.
pub const QUOTA_PROVIDERS: [(&str, &str); 26] = [
    ("claude", "Claude"),
    ("cline-pass", "ClinePass"),
    ("codex", "Codex"),
    ("cursor", "Cursor"),
    ("github-copilot", "GitHub Copilot"),
    ("google", "Google"),
    ("antigravity", "Antigravity"),
    ("kimi-for-coding", "Kimi for Coding"),
    ("nano-gpt", "NanoGPT"),
    ("openrouter", "OpenRouter"),
    ("zai-coding-plan", "z.ai"),
    ("zhipuai-coding-plan", "Zhipu AI Coding Plan"),
    (
        "minimax-cn-coding-plan",
        "MiniMax Coding Plan (minimaxi.com)",
    ),
    ("minimax-coding-plan", "MiniMax Coding Plan (minimax.io)"),
    ("ollama-cloud", "Ollama Cloud"),
    ("wafer", "Wafer.ai"),
    ("opencode-go", "OpenCode Go"),
    ("opencode", "OpenCode Zen"),
    ("deepinfra", "DeepInfra"),
    ("deepseek", "DeepSeek"),
    ("exe-dev", "exe.dev"),
    ("hyper", "Charm Hyper"),
    ("neuralwatt", "NeuralWatt"),
    ("kilo", "Kilo Code"),
    ("zenmux", "ZenMux"),
    ("xai", "xAI"),
];

/// A window's value besides its percentage.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowValue {
    Requests(f64),
    Spend { spent: f64, limit: Option<f64> },
}

/// One usage window (`5h`, `7d`, `daily`...).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageWindow {
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    /// Milliseconds since the epoch.
    pub reset_at: Option<f64>,
    pub reset_at_formatted: Option<String>,
    pub reset_after_formatted: Option<String>,
    pub value_label: Option<String>,
    pub value: Option<WindowValue>,
    pub credits_used: Option<f64>,
}

/// One provider's answer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QuotaResult {
    pub provider_id: String,
    pub provider_name: String,
    pub ok: bool,
    pub configured: bool,
    pub error: Option<String>,
    pub plan_label: Option<String>,
    /// `None` when the provider answered no usage.
    pub windows: Option<BTreeMap<String, UsageWindow>>,
    /// Per-model windows, by model name.
    pub models: BTreeMap<String, BTreeMap<String, UsageWindow>>,
    pub fetched_at: f64,
}

impl QuotaResult {
    /// `getUsagePercent`: the fullest window, the sidebar's dot.
    pub fn highest_used_percent(&self) -> Option<f64> {
        self.windows
            .as_ref()?
            .values()
            .filter_map(|window| window.used_percent)
            .reduce(f64::max)
    }
}

/// `resolveUsageTone`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageTone {
    Safe,
    Warn,
    Critical,
}

pub fn usage_tone(percent: Option<f64>) -> UsageTone {
    match percent {
        Some(percent) if percent >= 80.0 => UsageTone::Critical,
        Some(percent) if percent >= 50.0 => UsageTone::Warn,
        _ => UsageTone::Safe,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireWindow {
    used_percent: Option<f64>,
    remaining_percent: Option<f64>,
    #[serde(rename = "windowSeconds")]
    _window_seconds: Option<f64>,
    #[serde(rename = "resetAfterSeconds")]
    _reset_after_seconds: Option<f64>,
    reset_at: Option<f64>,
    reset_at_formatted: Option<String>,
    reset_after_formatted: Option<String>,
    #[serde(default)]
    value_label: Option<String>,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    credits_used: Option<f64>,
}

impl WireWindow {
    fn into_window(self) -> Result<UsageWindow, String> {
        let value = match self.value {
            None => None,
            Some(value) => Some(match value.get("kind").and_then(Value::as_str) {
                Some("requests") => WindowValue::Requests(
                    value
                        .get("count")
                        .and_then(Value::as_f64)
                        .filter(|count| *count >= 0.0)
                        .ok_or("invalid request count")?,
                ),
                Some("spend") => WindowValue::Spend {
                    spent: value
                        .get("spent")
                        .and_then(Value::as_f64)
                        .filter(|spent| *spent >= 0.0)
                        .ok_or("invalid spend")?,
                    limit: match value.get("limit") {
                        Some(Value::Null) | None => None,
                        Some(limit) => Some(
                            limit
                                .as_f64()
                                .filter(|limit| *limit > 0.0)
                                .ok_or("invalid limit")?,
                        ),
                    },
                },
                _ => return Err("unknown window value".into()),
            }),
        };
        Ok(UsageWindow {
            used_percent: self.used_percent,
            remaining_percent: self.remaining_percent,
            reset_at: self.reset_at,
            reset_at_formatted: self.reset_at_formatted,
            reset_after_formatted: self.reset_after_formatted,
            value_label: self.value_label,
            value,
            credits_used: self.credits_used,
        })
    }
}

#[derive(Deserialize)]
struct WireModel {
    windows: BTreeMap<String, WireWindow>,
}

#[derive(Deserialize)]
struct WireUsage {
    windows: BTreeMap<String, WireWindow>,
    #[serde(default)]
    models: Option<BTreeMap<String, WireModel>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireResult {
    provider_id: String,
    provider_name: String,
    ok: bool,
    configured: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    plan_label: Option<String>,
    usage: Option<WireUsage>,
    fetched_at: f64,
}

fn windows(wire: BTreeMap<String, WireWindow>) -> Result<BTreeMap<String, UsageWindow>, String> {
    wire.into_iter()
        .map(|(name, window)| Ok((name, window.into_window()?)))
        .collect()
}

/// Validates one answer for `provider`; the whole answer fails as the web's
/// schema does.
pub fn decode_result(provider: &str, payload: Value) -> Result<QuotaResult, TransportError> {
    let malformed = |error: String| TransportError::Malformed(format!("quota: {error}"));
    let wire: WireResult =
        serde_json::from_value(payload).map_err(|error| malformed(error.to_string()))?;
    if wire.provider_id != provider {
        return Err(malformed("another provider's answer".into()));
    }
    let (windows, models) = match wire.usage {
        None => (None, BTreeMap::new()),
        Some(usage) => (
            Some(windows(usage.windows).map_err(malformed)?),
            usage
                .models
                .unwrap_or_default()
                .into_iter()
                .map(|(name, model)| Ok((name, windows(model.windows)?)))
                .collect::<Result<_, String>>()
                .map_err(malformed)?,
        ),
    };
    Ok(QuotaResult {
        provider_id: wire.provider_id,
        provider_name: wire.provider_name,
        ok: wire.ok,
        configured: wire.configured,
        error: wire.error,
        plan_label: wire.plan_label,
        windows,
        models,
        fetched_at: wire.fetched_at,
    })
}

impl ServerClient {
    /// One provider's quota.
    pub async fn quota(&self, provider: &str) -> Result<QuotaResult, TransportError> {
        let mut url = self.endpoint().url("api/quota/");
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .pop_if_empty()
            .push(provider);
        let payload: Value = self.get_json(url, None).await?;
        decode_result(provider, payload)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn window(used: f64) -> Value {
        json!({
            "usedPercent": used, "remainingPercent": 100.0 - used, "windowSeconds": 18000,
            "resetAfterSeconds": 60, "resetAt": 1_700_000_000_000_u64,
            "resetAtFormatted": null, "resetAfterFormatted": "1m",
        })
    }

    #[test]
    fn results_decode_windows_models_and_the_fullest_window() {
        let result = decode_result(
            "codex",
            json!({
                "providerId": "codex", "providerName": "Codex", "ok": true, "configured": true,
                "usage": {
                    "windows": {"5h": window(40.0), "7d": window(85.0)},
                    "models": {"gpt": {"windows": {"5h": window(10.0)}}},
                },
                "fetchedAt": 1,
            }),
        )
        .expect("valid");
        assert_eq!(result.highest_used_percent(), Some(85.0));
        assert_eq!(
            usage_tone(result.highest_used_percent()),
            UsageTone::Critical
        );
        assert_eq!(usage_tone(Some(50.0)), UsageTone::Warn);
        assert_eq!(usage_tone(None), UsageTone::Safe);
        assert_eq!(result.models["gpt"]["5h"].used_percent, Some(10.0));
        assert_eq!(
            result.windows.as_ref().expect("usage")["5h"]
                .reset_after_formatted
                .as_deref(),
            Some("1m")
        );
    }

    #[test]
    fn unconfigured_and_failed_answers_stay_distinct() {
        let unconfigured = decode_result(
            "claude",
            json!({"providerId": "claude", "providerName": "Claude", "ok": false,
                   "configured": false, "usage": null, "fetchedAt": 1}),
        )
        .expect("valid");
        assert!(!unconfigured.configured);
        assert_eq!(unconfigured.windows, None);
        let failed = decode_result(
            "claude",
            json!({"providerId": "claude", "providerName": "Claude", "ok": false,
                   "configured": true, "error": "401", "usage": null, "fetchedAt": 1}),
        )
        .expect("valid");
        assert_eq!(failed.error.as_deref(), Some("401"));
        assert!(
            decode_result(
                "claude",
                json!({"providerId": "codex", "providerName": "Codex", "ok": true,
                       "configured": true, "usage": null, "fetchedAt": 1})
            )
            .is_err(),
            "another provider's answer is no answer"
        );
        let mut bad = window(1.0);
        bad["value"] = json!({"kind": "spend", "spent": -1});
        assert!(
            decode_result(
                "codex",
                json!({"providerId": "codex", "providerName": "Codex", "ok": true,
                       "configured": true, "usage": {"windows": {"5h": bad}}, "fetchedAt": 1})
            )
            .is_err()
        );
    }
}
