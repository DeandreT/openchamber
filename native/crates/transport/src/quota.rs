//! Provider quotas (`lib/quota`): `GET /api/quota/:providerId` answers one
//! provider's usage windows, as `fetchQuota.ts` validates them, and
//! `POST /api/quota/:providerId/gift-reset/use` spends a gift limit reset.
//!
//! Windows and models keep the order the server wrote them in: the compact
//! readouts show a model's first reported window, and a provider puts its
//! tightest window first.

use std::fmt;
use std::marker::PhantomData;

use reqwest::Method;
use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::{Value, json};

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::files::{parse, read_answer};

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

/// A gift limit reset a window can spend (`giftReset`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GiftReset {
    /// The provider's record of the gift.
    pub record_id: f64,
    /// When the gift expires, in milliseconds since the epoch.
    pub expire_at: f64,
}

/// Which limit a gift reset restores (`QuotaGiftResetType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GiftResetType {
    FiveHour,
    Week,
}

impl GiftResetType {
    /// `giftResetTypeForWindow`: only the five-hour and the weekly window
    /// have a reset.
    pub fn for_window_seconds(seconds: Option<f64>) -> Option<Self> {
        match seconds {
            Some(18_000.0) => Some(Self::FiveHour),
            Some(604_800.0) => Some(Self::Week),
            _ => None,
        }
    }

    fn wire_name(self) -> &'static str {
        match self {
            Self::FiveHour => "FIVE_HOUR",
            Self::Week => "WEEK",
        }
    }
}

/// Named entries in the order the server wrote them.
pub type Ordered<T> = Vec<(String, T)>;

/// One usage window (`5h`, `7d`, `daily`...).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageWindow {
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    /// The window's length in seconds; absent for balances and counters.
    pub window_seconds: Option<f64>,
    /// Milliseconds since the epoch.
    pub reset_at: Option<f64>,
    pub reset_at_formatted: Option<String>,
    pub reset_after_formatted: Option<String>,
    pub value_label: Option<String>,
    pub value: Option<WindowValue>,
    pub credits_used: Option<f64>,
    pub gift_reset: Option<GiftReset>,
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
    pub windows: Option<Ordered<UsageWindow>>,
    /// Per-model windows, by model name.
    pub models: Ordered<Ordered<UsageWindow>>,
    pub fetched_at: f64,
}

impl QuotaResult {
    /// `getUsagePercent`: the fullest window, the sidebar's dot.
    pub fn highest_used_percent(&self) -> Option<f64> {
        self.windows
            .as_ref()?
            .iter()
            .filter_map(|(_, window)| window.used_percent)
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

/// A JSON object's entries in encounter order; a repeated key keeps its
/// first place and its last value, as a JavaScript object does.
struct WireEntries<T>(Ordered<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for WireEntries<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntriesVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for EntriesVisitor<T> {
            type Value = WireEntries<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object of named entries")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut entries: Ordered<T> = Vec::new();
                while let Some((name, value)) = map.next_entry::<String, T>()? {
                    match entries.iter_mut().find(|(known, _)| *known == name) {
                        Some(entry) => entry.1 = value,
                        None => entries.push((name, value)),
                    }
                }
                Ok(WireEntries(entries))
            }
        }

        deserializer.deserialize_map(EntriesVisitor(PhantomData))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireGift {
    record_id: f64,
    expire_at: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireWindow {
    used_percent: Option<f64>,
    remaining_percent: Option<f64>,
    window_seconds: Option<f64>,
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
    #[serde(default)]
    gift_reset: Option<WireGift>,
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
            window_seconds: self.window_seconds,
            reset_at: self.reset_at,
            reset_at_formatted: self.reset_at_formatted,
            reset_after_formatted: self.reset_after_formatted,
            value_label: self.value_label,
            value,
            credits_used: self.credits_used,
            gift_reset: self.gift_reset.map(|gift| GiftReset {
                record_id: gift.record_id,
                expire_at: gift.expire_at,
            }),
        })
    }
}

#[derive(Deserialize)]
struct WireModel {
    windows: WireEntries<WireWindow>,
}

#[derive(Deserialize)]
struct WireUsage {
    windows: WireEntries<WireWindow>,
    #[serde(default)]
    models: Option<WireEntries<WireModel>>,
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

fn windows(wire: WireEntries<WireWindow>) -> Result<Ordered<UsageWindow>, String> {
    wire.0
        .into_iter()
        .map(|(name, window)| Ok((name, window.into_window()?)))
        .collect()
}

fn malformed(error: String) -> TransportError {
    TransportError::Malformed(format!("quota: {error}"))
}

/// Checks that `wire` answers for `provider` and projects it; the whole
/// answer fails as the web's schema does.
fn project(provider: &str, wire: WireResult) -> Result<QuotaResult, TransportError> {
    if wire.provider_id != provider {
        return Err(malformed("another provider's answer".into()));
    }
    let (windows, models) = match wire.usage {
        None => (None, Vec::new()),
        Some(usage) => (
            Some(windows(usage.windows).map_err(malformed)?),
            usage
                .models
                .map(|models| models.0)
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

/// Validates one answer for `provider`. A [`Value`] holds its object keys
/// sorted, so windows and models decoded through here are in name order;
/// [`ServerClient::quota`] decodes the response body and keeps the server's.
pub fn decode_result(provider: &str, payload: Value) -> Result<QuotaResult, TransportError> {
    let wire: WireResult =
        serde_json::from_value(payload).map_err(|error| malformed(error.to_string()))?;
    project(provider, wire)
}

#[derive(Deserialize)]
struct GiftReceipt {
    success: bool,
}

/// Why a gift reset did not end as a confirmed success.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GiftResetError {
    /// The server answered that the reset failed, with its own explanation
    /// when it gave one (`{ error }`, as `activateGiftReset` reads it).
    #[error("the server did not activate the gift reset (HTTP {status})")]
    Refused {
        status: u16,
        message: Option<String>,
    },
    /// No usable answer arrived, so nothing says whether the gift was spent.
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl GiftResetError {
    /// The server's own explanation, when it gave one.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Refused { message, .. } => message.as_deref(),
            Self::Transport(_) => None,
        }
    }

    /// Whether the server rejected the request itself (a 4xx answer), so the
    /// provider was never asked to spend the gift. A provider failure the
    /// server relays (5xx) and every transport failure leave that open.
    pub fn is_definitive(&self) -> bool {
        matches!(self, Self::Refused { status, .. } if (400..500).contains(status))
    }
}

impl ServerClient {
    fn quota_url(&self, provider: &str, rest: &[&str]) -> reqwest::Url {
        let mut url = self.endpoint().url("api/quota/");
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .pop_if_empty()
            .push(provider)
            .extend(rest);
        url
    }

    /// One provider's quota.
    pub async fn quota(&self, provider: &str) -> Result<QuotaResult, TransportError> {
        let wire: WireResult = self.get_json(self.quota_url(provider, &[]), None).await?;
        project(provider, wire)
    }

    /// Spends the gift limit reset `record_id` of `provider`. Only an answer
    /// that says it succeeded is a success; the request is never repeated.
    pub async fn use_gift_reset(
        &self,
        provider: &str,
        record_id: f64,
        reset: GiftResetType,
    ) -> Result<(), GiftResetError> {
        let response = self
            .authorized(
                Method::POST,
                self.quota_url(provider, &["gift-reset", "use"]),
            )
            .json(&json!({ "recordId": record_id, "resetType": reset.wire_name() }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let body = match read_answer(response).await? {
            Ok(body) => body,
            Err(refusal) => {
                return Err(GiftResetError::Refused {
                    status: refusal.status.as_u16(),
                    message: refusal.message.filter(|message| !message.is_empty()),
                });
            }
        };
        let receipt: GiftReceipt = parse(&body)?;
        if receipt.success {
            Ok(())
        } else {
            Err(malformed("the gift reset was not confirmed".into()).into())
        }
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(result.models[0].0, "gpt");
        assert_eq!(result.models[0].1[0].1.used_percent, Some(10.0));
        let first = &result.windows.as_ref().expect("usage")[0];
        assert_eq!(first.0, "5h");
        assert_eq!(first.1.reset_after_formatted.as_deref(), Some("1m"));
        assert_eq!(first.1.window_seconds, Some(18_000.0));
    }

    #[test]
    fn windows_and_models_keep_the_order_the_server_wrote() {
        // `weekly` sorts after `5h` and `b-model` after `a-model`; the server
        // put the bottleneck first.
        let body = r#"{
            "providerId": "antigravity", "providerName": "Antigravity", "ok": true,
            "configured": true, "fetchedAt": 1,
            "usage": {
                "windows": {
                    "weekly": {"usedPercent": 90, "remainingPercent": 10, "windowSeconds": 604800,
                        "resetAfterSeconds": null, "resetAt": null, "resetAtFormatted": null,
                        "resetAfterFormatted": null,
                        "giftReset": {"recordId": 7, "expireAt": 1700000000000}},
                    "5h": {"usedPercent": 5, "remainingPercent": 95, "windowSeconds": 18000,
                        "resetAfterSeconds": null, "resetAt": null, "resetAtFormatted": null,
                        "resetAfterFormatted": null}
                },
                "models": {
                    "b-model": {"windows": {
                        "weekly": {"usedPercent": 1, "remainingPercent": 99, "windowSeconds": null,
                            "resetAfterSeconds": null, "resetAt": null, "resetAtFormatted": null,
                            "resetAfterFormatted": null},
                        "5h": {"usedPercent": 2, "remainingPercent": 98, "windowSeconds": null,
                            "resetAfterSeconds": null, "resetAt": null, "resetAtFormatted": null,
                            "resetAfterFormatted": null}
                    }},
                    "a-model": {"windows": {}}
                }
            }
        }"#;
        let wire: WireResult = serde_json::from_str(body).expect("valid");
        let result = project("antigravity", wire).expect("valid");
        let windows = result.windows.expect("usage");
        let names = |entries: &[(String, UsageWindow)]| -> Vec<String> {
            entries.iter().map(|(name, _)| name.clone()).collect()
        };
        assert_eq!(names(windows.as_slice()), ["weekly", "5h"]);
        assert_eq!(
            windows[0].1.gift_reset,
            Some(GiftReset {
                record_id: 7.0,
                expire_at: 1_700_000_000_000.0
            })
        );
        assert_eq!(windows[1].1.gift_reset, None);
        assert_eq!(result.models[0].0, "b-model");
        assert_eq!(names(result.models[0].1.as_slice()), ["weekly", "5h"]);
        assert_eq!(result.models[1].0, "a-model");
        assert_eq!(
            GiftResetType::for_window_seconds(windows[0].1.window_seconds),
            Some(GiftResetType::Week)
        );
        assert_eq!(
            GiftResetType::for_window_seconds(Some(18_000.0)),
            Some(GiftResetType::FiveHour)
        );
        assert_eq!(GiftResetType::for_window_seconds(Some(86_400.0)), None);
        assert_eq!(GiftResetType::for_window_seconds(None), None);
    }

    #[tokio::test]
    async fn quota_reads_one_provider_and_gift_resets_need_a_confirmed_receipt() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/api/quota/codex") => (
                    200,
                    json!({"providerId": "codex", "providerName": "Codex", "ok": true,
                           "configured": true, "usage": {"windows": {"5h": window(40.0)}},
                           "fetchedAt": 1}),
                ),
                // Another provider's answer under the asked path.
                ("GET", "/api/quota/claude") => (
                    200,
                    json!({"providerId": "codex", "providerName": "Codex", "ok": true,
                           "configured": true, "usage": null, "fetchedAt": 1}),
                ),
                ("GET", _) => (500, json!({"error": "boom"})),
                ("POST", "/api/quota/zai-coding-plan/gift-reset/use") => {
                    (200, json!({"success": request.body["recordId"] == 7.0}))
                }
                ("POST", "/api/quota/claude/gift-reset/use") => (
                    404,
                    json!({"code": "UNSUPPORTED_PROVIDER", "error": "Unsupported quota provider"}),
                ),
                ("POST", "/api/quota/codex/gift-reset/use") => (400, json!({})),
                _ => (502, json!({"error": "provider failed"})),
            }
        }))
        .await;
        let codex = client.quota("codex").await.expect("answered");
        assert_eq!(codex.highest_used_percent(), Some(40.0));
        assert!(matches!(
            client.quota("claude").await,
            Err(TransportError::Malformed(_))
        ));
        assert!(
            matches!(
                client.quota("cursor").await,
                Err(TransportError::Http { status: 500 })
            ),
            "a failed read is an error, never an unconfigured answer"
        );
        client
            .use_gift_reset("zai-coding-plan", 7.0, GiftResetType::Week)
            .await
            .expect("confirmed");
        let unconfirmed = client
            .use_gift_reset("zai-coding-plan", 8.0, GiftResetType::FiveHour)
            .await
            .expect_err("an unconfirmed receipt is a failure");
        assert!(matches!(
            unconfirmed,
            GiftResetError::Transport(TransportError::Malformed(_))
        ));
        assert!(
            !unconfirmed.is_definitive(),
            "nothing says whether the gift was spent"
        );
        let relayed = client
            .use_gift_reset("zai", 7.0, GiftResetType::Week)
            .await
            .expect_err("the provider failed");
        assert!(matches!(
            relayed,
            GiftResetError::Refused { status: 502, .. }
        ));
        assert_eq!(
            relayed.message(),
            Some("provider failed"),
            "the server's own explanation is kept"
        );
        assert!(
            !relayed.is_definitive(),
            "a relayed provider failure may have spent it"
        );
        let unsupported = client
            .use_gift_reset("claude", 7.0, GiftResetType::Week)
            .await
            .expect_err("unsupported");
        assert_eq!(unsupported.message(), Some("Unsupported quota provider"));
        assert!(unsupported.is_definitive());
        let unexplained = client
            .use_gift_reset("codex", 7.0, GiftResetType::Week)
            .await
            .expect_err("rejected");
        assert_eq!(unexplained.message(), None);
        assert!(unexplained.is_definitive());
        assert!(
            !unexplained.to_string().is_empty(),
            "a rejection without an explanation still has text"
        );
        let recorded = recorded.lock().expect("log");
        let posts: Vec<_> = recorded
            .iter()
            .filter(|request| request.method == "POST")
            .collect();
        assert_eq!(posts[0].path, "/api/quota/zai-coding-plan/gift-reset/use");
        assert_eq!(posts[0].body, json!({"recordId": 7.0, "resetType": "WEEK"}));
        assert_eq!(
            posts[1].body,
            json!({"recordId": 8.0, "resetType": "FIVE_HOUR"})
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
