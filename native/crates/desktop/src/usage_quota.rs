//! The selected runtime's provider quotas (`useQuotaStore.ts`), read once and
//! shown by Settings' Usage page and the work status card's Usage section.
//!
//! Each provider is read on its own (`GET /api/quota/:providerId`) and a read
//! in flight is shared, so one failing provider leaves the others' answers
//! and two readouts never ask twice. A failed read keeps the last answer and
//! records why; only a provider's own answer says it is not configured.
//! Another runtime starts empty, and a read started for the one left behind
//! changes nothing.
//!
//! The card asks for the providers the saved settings select, when it comes
//! on screen, when its window is presented again, and every three minutes
//! while both hold. Once the runtime answered and nothing selected is
//! configured, the card shows no readout and the reads stop.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::{Context, Task};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_transport::client::ServerSettings;
use openchamber_native_transport::quota::{
    GiftResetType, QUOTA_PROVIDERS, QuotaResult, UsageWindow,
};

use crate::locale::Strings;
use crate::network::NetworkTask;
use crate::settings_controls::{ServerTarget, same_target};

/// `QUOTA_REFRESH_INTERVAL_MS`: how often a shown readout reads again.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(3 * 60);
/// `QUOTA_OPEN_FRESHNESS_MS`: a readout shown again this soon reads nothing.
pub(crate) const OPEN_FRESHNESS: Duration = Duration::from_secs(15);
/// The web's text for a failure that carries none.
const FAILED: &str = "Failed to fetch quota";

/// Whether readouts show the used or the remaining share (`usageDisplayMode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DisplayMode {
    #[default]
    Usage,
    Remaining,
}

impl DisplayMode {
    /// The share of `window` this mode shows.
    pub(crate) fn percent(self, window: &UsageWindow) -> Option<f64> {
        match self {
            Self::Usage => window.used_percent,
            Self::Remaining => window.remaining_percent,
        }
    }
}

/// The saved usage preferences (`parseSettings`), with the web's defaults
/// for what the settings document leaves out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UsagePreferences {
    pub(crate) mode: DisplayMode,
    /// The selected quota providers, in the registry's order.
    pub(crate) providers: Vec<&'static str>,
    /// Per provider, the models the compact readouts show.
    pub(crate) models: Vec<(String, Vec<String>)>,
}

impl UsagePreferences {
    /// Without a settings document every provider is selected, as the web
    /// keeps its defaults when the settings read fails.
    pub(crate) fn from_settings(settings: Option<&ServerSettings>) -> Self {
        let saved = settings.and_then(|settings| settings.usage_providers.as_ref());
        Self {
            mode: match settings.and_then(|settings| settings.usage_remaining) {
                Some(true) => DisplayMode::Remaining,
                _ => DisplayMode::Usage,
            },
            providers: QUOTA_PROVIDERS
                .iter()
                .map(|(provider, _)| *provider)
                .filter(|provider| {
                    saved.is_none_or(|saved| saved.iter().any(|entry| entry == provider))
                })
                .collect(),
            models: settings
                .map(|settings| settings.usage_selected_models.clone())
                .unwrap_or_default(),
        }
    }

    fn selected_models(&self, provider: &str) -> &[String] {
        self.models
            .iter()
            .find(|(known, _)| known == provider)
            .map(|(_, models)| models.as_slice())
            .unwrap_or_default()
    }
}

/// One provider's last answer and its newest refresh failure.
#[derive(Clone, Debug, Default)]
pub(crate) struct ProviderState {
    pub(crate) result: Option<QuotaResult>,
    pub(crate) refresh_error: Option<String>,
}

impl ProviderState {
    /// `fetchProviderQuota`: a configured provider's failed answer keeps the
    /// last configured sample, and a failed read keeps whatever was held.
    pub(crate) fn apply(&mut self, answer: Result<QuotaResult, String>) {
        match answer {
            Ok(result) if !result.ok && result.configured => {
                self.refresh_error = Some(result.error.clone().unwrap_or_else(|| FAILED.into()));
                let keeps_previous = self
                    .result
                    .as_ref()
                    .is_some_and(|previous| previous.configured);
                if !keeps_previous {
                    self.result = Some(result);
                }
            }
            Ok(result) => {
                self.refresh_error = None;
                self.result = Some(result);
            }
            Err(error) => self.refresh_error = Some(error),
        }
    }

    /// The readout's error line: stale usage says it is stale.
    pub(crate) fn error_line(&self, strings: &Strings) -> Option<String> {
        let usage = self
            .result
            .as_ref()
            .is_some_and(|result| result.windows.is_some());
        match &self.refresh_error {
            Some(error) if usage => Some(
                strings
                    .format(
                        "header.services.usageRefreshFailedStale",
                        &[("error", error)],
                    )
                    .to_string(),
            ),
            Some(error) => Some(error.clone()),
            None => None,
        }
    }

    /// Whether the compact readouts list the provider: it is configured, or
    /// its first read failed and its configuration is still unknown.
    fn listed(&self) -> bool {
        match &self.result {
            Some(result) => result.configured,
            None => self.refresh_error.is_some(),
        }
    }
}

/// One limit of a provider in the compact readouts (`UsageLimitRow`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UsageRow {
    pub(crate) key: String,
    /// The window's label, in the interface language.
    pub(crate) label: String,
    /// The model the window belongs to; `None` for the provider's own.
    pub(crate) subtitle: Option<String>,
    pub(crate) window: UsageWindow,
}

/// One provider in the compact readouts (`UsageProviderGroup`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UsageGroup {
    pub(crate) provider_id: &'static str,
    pub(crate) provider_name: &'static str,
    pub(crate) rows: Vec<UsageRow>,
    /// A read failure, the provider's own error, or "nothing reported".
    pub(crate) status: Option<String>,
}

/// `getDisplayModelName`: the name without its sign-in source.
fn display_model_name(model: &str) -> &str {
    match model.split_once('/') {
        Some(("gemini" | "antigravity", name)) => name,
        _ => model,
    }
}

/// `resolveQuotaProviderId`: the quota provider behind a model provider.
pub(crate) fn quota_provider_id(model_provider: &str) -> Option<String> {
    let normalized = model_provider.trim().to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    Some(
        match normalized.as_str() {
            "openai" | "chatgpt" => "codex",
            "anthropic" | "claude-agent" | "claude-code" => "claude",
            "gemini" => "google",
            "kilocode" | "kilo-code" => "kilo",
            other => other,
        }
        .to_owned(),
    )
}

/// `pickUsageHeadline`: the shortest window of the provider the composer
/// points at. A provider's own windows come before its models'; a row without
/// a length is the last resort. `None` when no listed provider matches.
pub(crate) fn headline<'a>(
    groups: &'a [UsageGroup],
    model_provider: Option<&str>,
) -> Option<&'a UsageRow> {
    let provider = quota_provider_id(model_provider?)?;
    let group = groups.iter().find(|group| group.provider_id == provider)?;
    let own = group.rows.iter().any(|row| row.subtitle.is_none());
    let mut best: Option<(&UsageRow, f64)> = None;
    let mut first = None;
    for row in &group.rows {
        if own && row.subtitle.is_some() {
            continue;
        }
        first = first.or(Some(row));
        let Some(seconds) = row.window.window_seconds else {
            continue;
        };
        if !seconds.is_finite() || seconds <= 0.0 {
            continue;
        }
        if best.is_none_or(|(_, shortest)| seconds < shortest) {
            best = Some((row, seconds));
        }
    }
    best.map(|(row, _)| row).or(first)
}

/// What the work status card asks of the quotas.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CardDemand {
    /// The card's Usage section is mounted in the workspace.
    pub(crate) shown: bool,
    /// The providers the saved settings select.
    pub(crate) providers: Vec<&'static str>,
}

/// A gift reset in flight: the provider and the gift's record.
type GiftKey = (&'static str, u64);

/// How an accepted gift reset ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GiftOutcome {
    /// The server confirmed it spent the gift.
    Spent,
    /// The server rejected the request itself; nothing changed.
    Declined,
    /// No answer says whether the gift was spent.
    Unknown,
}

/// What an accepted gift reset runs against.
pub(crate) struct GiftTicket {
    pub(crate) target: ServerTarget,
    epoch: u64,
    key: GiftKey,
}

#[derive(Default)]
pub(crate) struct UsageQuota {
    runtime: Option<RuntimeId>,
    target: Option<ServerTarget>,
    /// Grows whenever in-flight reads stop describing the current server.
    epoch: u64,
    providers: HashMap<&'static str, ProviderState>,
    /// Each provider's read in flight, with the serial that names it.
    reads: HashMap<&'static str, (u64, Task<()>)>,
    read_serial: u64,
    /// A provider answered since the reads last drained.
    answered: bool,
    /// `loadedRuntimeKey`: the runtime answered a batch of reads.
    loaded: bool,
    all_wanted: bool,
    all_requested: bool,
    last_updated: Option<i64>,
    /// The mode chosen on the Usage page in this run; `None` follows the
    /// saved setting.
    display_mode: Option<DisplayMode>,
    card: CardDemand,
    /// Whether the platform presents the workspace's window.
    window_visible: bool,
    refresh_timer: Option<Task<()>>,
    last_visible_refresh: Option<Instant>,
    gifts: HashSet<GiftKey>,
}

impl UsageQuota {
    /// The quotas of a workspace whose window is presented or not.
    pub(crate) fn new(window_visible: bool) -> Self {
        Self {
            window_visible,
            ..Self::default()
        }
    }

    /// Follows the window's visibility (`updateQuotaAutoRefresh` on every
    /// visibility change): a hidden window stops the timed reads, and one
    /// presented again reads at once unless it read within the freshness
    /// window.
    pub(crate) fn set_window_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.window_visible == visible {
            return;
        }
        self.window_visible = visible;
        self.reconcile(cx);
        cx.notify();
    }

    /// Whether `synchronize` with the same arguments would change nothing.
    pub(crate) fn matches(
        &self,
        runtime: &RuntimeId,
        target: Option<&ServerTarget>,
        card: &CardDemand,
    ) -> bool {
        self.runtime.as_ref() == Some(runtime)
            && same_target(self.target.as_ref(), target)
            && self.card == *card
    }

    /// Adopts the workspace's runtime, connection and card. Another runtime
    /// forgets every sample (`resetForRuntimeSwitch`); a new connection to
    /// the same runtime keeps them and retires the reads in flight.
    pub(crate) fn synchronize(
        &mut self,
        runtime: RuntimeId,
        target: Option<ServerTarget>,
        card: CardDemand,
        cx: &mut Context<Self>,
    ) {
        if self.runtime.as_ref() != Some(&runtime) {
            self.retire_reads();
            self.providers.clear();
            self.loaded = false;
            self.all_wanted = false;
            self.all_requested = false;
            self.last_updated = None;
            self.display_mode = None;
            self.refresh_timer = None;
            self.runtime = Some(runtime);
        } else if !same_target(self.target.as_ref(), target.as_ref()) {
            self.retire_reads();
            self.refresh_timer = None;
            // The Usage page's read of every provider was retired with the
            // rest, so the new connection is asked again.
            if self.all_wanted {
                self.all_requested = false;
            }
        }
        self.target = target;
        self.card = card;
        self.reconcile(cx);
        self.read_unknown(cx);
        cx.notify();
    }

    /// Reads the card's providers that have neither an answer nor a read in
    /// flight: one the saved selection gained after the runtime answered
    /// would otherwise wait for a timed read that a card without readouts
    /// never makes.
    fn read_unknown(&mut self, cx: &mut Context<Self>) {
        if !self.card.shown || !self.window_visible {
            return;
        }
        let unknown: Vec<&'static str> = self
            .card
            .providers
            .iter()
            .copied()
            .filter(|provider| {
                !self.providers.contains_key(provider) && !self.reads.contains_key(provider)
            })
            .collect();
        if !unknown.is_empty() {
            self.refresh(&unknown, cx);
        }
    }

    fn retire_reads(&mut self) {
        self.epoch += 1;
        self.reads.clear();
        self.answered = false;
        self.last_visible_refresh = None;
    }

    /// `useQuotaAutoRefresh` for the card: shown in a presented window, and
    /// either listing a provider or still waiting for the runtime's first
    /// answer.
    fn card_demands(&self) -> bool {
        self.card.shown
            && self.window_visible
            && self.target.is_some()
            && (!self.loaded || self.lists_any(&self.card.providers))
    }

    fn lists_any(&self, providers: &[&'static str]) -> bool {
        providers.iter().any(|provider| {
            self.providers
                .get(provider)
                .is_some_and(ProviderState::listed)
        })
    }

    /// The providers of `providers` the compact readouts list.
    pub(crate) fn listed_providers(&self, providers: &[&'static str]) -> Vec<&'static str> {
        providers
            .iter()
            .copied()
            .filter(|provider| {
                self.providers
                    .get(provider)
                    .is_some_and(ProviderState::listed)
            })
            .collect()
    }

    /// Starts or stops the card's reads to follow its demand, and starts the
    /// Usage page's read of every provider once there is a server to ask.
    fn reconcile(&mut self, cx: &mut Context<Self>) {
        if self.all_wanted && !self.all_requested && self.target.is_some() {
            self.all_requested = true;
            self.refresh_all(cx);
        }
        if !self.card_demands() {
            self.refresh_timer = None;
            return;
        }
        if self.refresh_timer.is_some() {
            return;
        }
        self.refresh_visible(cx);
        self.refresh_timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let alive = this.update(cx, |this, cx| this.refresh_visible(cx));
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    /// `refreshVisibleQuotas`: reads the card's providers unless they were
    /// read within the freshness window.
    fn refresh_visible(&mut self, cx: &mut Context<Self>) {
        if !self.card_demands() {
            return;
        }
        let now = cx.background_executor().now();
        if self
            .last_visible_refresh
            .is_some_and(|at| now.duration_since(at) < OPEN_FRESHNESS)
        {
            return;
        }
        self.last_visible_refresh = Some(now);
        let providers = self.card.providers.clone();
        self.refresh(&providers, cx);
    }

    /// Reads `providers`; one already being read joins its read.
    pub(crate) fn refresh(&mut self, providers: &[&'static str], cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        for &provider in providers {
            if !self.reads.contains_key(provider) {
                self.start_read(provider, &target, cx);
            }
        }
        cx.notify();
    }

    /// Reads `provider` now, in place of a read already in flight: that one
    /// was asked before something changed on the server and must not answer.
    fn reread(&mut self, provider: &'static str, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        self.start_read(provider, &target, cx);
        cx.notify();
    }

    /// Starts `provider`'s read under a new serial. A read it replaces is
    /// dropped, and its completion would no longer carry the held serial.
    fn start_read(
        &mut self,
        provider: &'static str,
        target: &ServerTarget,
        cx: &mut Context<Self>,
    ) {
        let epoch = self.epoch;
        self.read_serial += 1;
        let serial = self.read_serial;
        let client = target.client.clone();
        let read =
            NetworkTask::spawn_on(&target.network, async move { client.quota(provider).await });
        let task = cx.spawn(async move |this, cx| {
            let answer = match read.await {
                Some(Ok(result)) => Ok(result),
                Some(Err(error)) => Err(error.to_string()),
                None => Err(FAILED.to_owned()),
            };
            let _ = this.update(cx, |this, cx| {
                this.settle(epoch, serial, provider, answer, cx);
            });
        });
        self.reads.insert(provider, (serial, task));
    }

    /// Every provider again (`fetchAllQuotas`).
    pub(crate) fn refresh_all(&mut self, cx: &mut Context<Self>) {
        let providers: Vec<&'static str> = QUOTA_PROVIDERS
            .iter()
            .map(|(provider, _)| *provider)
            .collect();
        self.refresh(&providers, cx);
    }

    /// The Usage page reads every provider once per runtime.
    pub(crate) fn ensure_all(&mut self, cx: &mut Context<Self>) {
        self.all_wanted = true;
        self.reconcile(cx);
    }

    fn settle(
        &mut self,
        epoch: u64,
        serial: u64,
        provider: &'static str,
        answer: Result<QuotaResult, String>,
        cx: &mut Context<Self>,
    ) {
        // A read of the runtime or connection left behind describes nothing
        // shown now.
        if epoch != self.epoch {
            return;
        }
        // Only the read still held for the provider answers for it.
        if self.reads.get(provider).map(|(held, _)| *held) != Some(serial) {
            return;
        }
        if let Some((_, read)) = self.reads.remove(provider) {
            // This is the read's own completion; it only has to finish.
            read.detach();
        }
        self.answered |= answer.is_ok();
        let sample = matches!(&answer, Ok(result) if result.ok || !result.configured);
        self.providers.entry(provider).or_default().apply(answer);
        if sample {
            self.last_updated = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .map(|elapsed| elapsed.as_millis() as i64);
        }
        // The runtime is loaded only once it actually answered: a batch that
        // failed whole is asked again.
        if self.reads.is_empty() && std::mem::take(&mut self.answered) {
            self.loaded = true;
        }
        self.reconcile(cx);
        cx.notify();
    }

    /// Whether any provider is being read (`isLoading`).
    pub(crate) fn is_loading(&self) -> bool {
        !self.reads.is_empty()
    }

    pub(crate) fn provider(&self, provider: &str) -> Option<&ProviderState> {
        self.providers.get(provider)
    }

    /// How many providers have an answer or a failure.
    pub(crate) fn provider_count(&self) -> usize {
        self.providers.len()
    }

    /// When a provider last answered with a sample, in epoch milliseconds.
    pub(crate) fn last_updated(&self) -> Option<i64> {
        self.last_updated
    }

    /// The mode readouts show: this run's choice, else the saved one.
    pub(crate) fn display_mode(&self, saved: DisplayMode) -> DisplayMode {
        self.display_mode.unwrap_or(saved)
    }

    pub(crate) fn set_display_mode(&mut self, mode: DisplayMode, cx: &mut Context<Self>) {
        self.display_mode = Some(mode);
        cx.notify();
    }

    /// `useUsageProviderGroups`: the selected providers that are configured
    /// or whose first read failed, in the registry's order, each with its
    /// windows in the server's order and then one window per shown model.
    pub(crate) fn groups(
        &self,
        preferences: &UsagePreferences,
        strings: &Strings,
    ) -> Vec<UsageGroup> {
        let mut groups = Vec::new();
        for (provider_id, provider_name) in QUOTA_PROVIDERS {
            if !preferences.providers.contains(&provider_id) {
                continue;
            }
            let Some(state) = self
                .providers
                .get(provider_id)
                .filter(|state| state.listed())
            else {
                continue;
            };
            let result = state.result.as_ref();
            let mut rows = Vec::new();
            for (label, window) in result
                .and_then(|result| result.windows.as_ref())
                .into_iter()
                .flatten()
            {
                rows.push(UsageRow {
                    key: format!("window-{label}"),
                    label: crate::usage_page::window_label(label, strings),
                    subtitle: None,
                    window: window.clone(),
                });
            }
            let selected = preferences.selected_models(provider_id);
            for (model, windows) in result
                .map(|result| result.models.as_slice())
                .unwrap_or_default()
            {
                if !selected.is_empty() && !selected.contains(model) {
                    continue;
                }
                let Some((label, window)) = windows.first() else {
                    continue;
                };
                rows.push(UsageRow {
                    key: format!("model-{model}-{label}"),
                    label: crate::usage_page::window_label(label, strings),
                    subtitle: Some(display_model_name(model).to_owned()),
                    window: window.clone(),
                });
            }
            let own_error = result
                .filter(|result| !result.ok)
                .and_then(|result| result.error.clone());
            let status = state.error_line(strings).or(own_error).or_else(|| {
                rows.is_empty().then(|| {
                    strings
                        .get("header.services.noRateLimitsReported")
                        .to_string()
                })
            });
            groups.push(UsageGroup {
                provider_id,
                provider_name,
                rows,
                status,
            });
        }
        groups
    }

    /// Whether the gift reset `record_id` of `provider` is being spent.
    pub(crate) fn gift_pending(&self, provider: &str, record_id: f64) -> bool {
        self.gifts
            .iter()
            .any(|(known, record)| *known == provider && *record == record_id.to_bits())
    }

    /// Accepts one gift reset: the server it runs against, or `None` while
    /// the same gift is already being spent or there is no server.
    pub(crate) fn begin_gift(
        &mut self,
        provider: &'static str,
        record_id: f64,
        cx: &mut Context<Self>,
    ) -> Option<GiftTicket> {
        let target = self.target.clone()?;
        let key = (provider, record_id.to_bits());
        if !self.gifts.insert(key) {
            return None;
        }
        cx.notify();
        Some(GiftTicket {
            target,
            epoch: self.epoch,
            key,
        })
    }

    /// Settles an accepted gift reset. Unless the server declined the
    /// request itself, its provider is read again in place of any read asked
    /// before the gift settled, so the button follows what the server holds
    /// now. A server changed meanwhile is not asked.
    pub(crate) fn finish_gift(
        &mut self,
        ticket: &GiftTicket,
        outcome: GiftOutcome,
        cx: &mut Context<Self>,
    ) {
        self.gifts.remove(&ticket.key);
        if outcome != GiftOutcome::Declined && ticket.epoch == self.epoch {
            self.reread(ticket.key.0, cx);
        }
        cx.notify();
    }

    /// The serial of `provider`'s read in flight.
    #[cfg(test)]
    pub(crate) fn read_in_flight(&self, provider: &str) -> Option<u64> {
        self.reads.get(provider).map(|(serial, _)| *serial)
    }
}

/// The reset a window's gift restores, when the window has both.
pub(crate) fn gift_reset_type(window: &UsageWindow) -> Option<GiftResetType> {
    window.gift_reset?;
    GiftResetType::for_window_seconds(window.window_seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(used: f64, seconds: Option<f64>) -> UsageWindow {
        UsageWindow {
            used_percent: Some(used),
            remaining_percent: Some(100.0 - used),
            window_seconds: seconds,
            ..UsageWindow::default()
        }
    }

    fn answer(provider: &str, configured: bool, ok: bool) -> QuotaResult {
        QuotaResult {
            provider_id: provider.into(),
            provider_name: provider.into(),
            ok,
            configured,
            error: (!ok && configured).then(|| "429".into()),
            windows: (ok && configured).then(Vec::new),
            ..QuotaResult::default()
        }
    }

    fn quota(states: Vec<(&'static str, Result<QuotaResult, String>)>) -> UsageQuota {
        let mut quota = UsageQuota::default();
        for (provider, state) in states {
            quota.providers.entry(provider).or_default().apply(state);
        }
        quota
    }

    fn all() -> UsagePreferences {
        UsagePreferences::from_settings(None)
    }

    #[test]
    fn preferences_default_to_every_provider_and_keep_an_empty_selection() {
        let defaults = all();
        assert_eq!(defaults.mode, DisplayMode::Usage);
        assert_eq!(defaults.providers.len(), QUOTA_PROVIDERS.len());
        let saved = ServerSettings {
            usage_remaining: Some(true),
            usage_providers: Some(vec!["codex".into(), "unknown".into(), "claude".into()]),
            ..ServerSettings::default()
        };
        let preferences = UsagePreferences::from_settings(Some(&saved));
        assert_eq!(preferences.mode, DisplayMode::Remaining);
        assert_eq!(
            preferences.providers,
            ["claude", "codex"],
            "the registry's order, without unknown providers"
        );
        let none = ServerSettings {
            usage_providers: Some(Vec::new()),
            ..ServerSettings::default()
        };
        assert!(
            UsagePreferences::from_settings(Some(&none))
                .providers
                .is_empty(),
            "an empty saved list selects nothing"
        );
    }

    #[test]
    fn groups_list_configured_providers_and_first_read_failures_only() {
        let strings = Strings::load("en");
        let mut populated = answer("codex", true, true);
        populated.windows = Some(vec![
            ("weekly".into(), window(90.0, Some(604_800.0))),
            ("5h".into(), window(10.0, Some(18_000.0))),
        ]);
        let quota = quota(vec![
            ("claude", Ok(answer("claude", false, false))),
            ("codex", Ok(populated)),
            ("cursor", Err("connection refused".into())),
            ("google", Ok(answer("google", true, false))),
            ("openrouter", Ok(answer("openrouter", true, true))),
        ]);
        let groups = quota.groups(&all(), &strings);
        let ids: Vec<&str> = groups.iter().map(|group| group.provider_id).collect();
        assert_eq!(
            ids,
            ["codex", "cursor", "google", "openrouter"],
            "the registry's order; the unconfigured provider is absent"
        );
        let labels: Vec<&str> = groups[0]
            .rows
            .iter()
            .map(|row| row.label.as_str())
            .collect();
        assert_eq!(labels, ["Weekly", "5-Hour"], "the server's order");
        assert_eq!(groups[0].status, None);
        assert_eq!(
            groups[1].status.as_deref(),
            Some("connection refused"),
            "a first read that failed is listed with its failure"
        );
        assert!(groups[1].rows.is_empty());
        assert_eq!(groups[2].status.as_deref(), Some("429"));
        assert_eq!(
            groups[3].status.as_deref(),
            Some("No rate limits reported."),
            "a configured provider without windows stays listed"
        );
        let selected = UsagePreferences {
            providers: vec!["cursor", "google"],
            ..all()
        };
        let ids: Vec<&str> = quota
            .groups(&selected, &strings)
            .iter()
            .map(|group| group.provider_id)
            .collect();
        assert_eq!(ids, ["cursor", "google"]);
        assert!(UsageQuota::default().groups(&all(), &strings).is_empty());
    }

    #[test]
    fn a_failed_refresh_keeps_rows_and_one_failure_leaves_the_other_providers() {
        let strings = Strings::load("en");
        let mut codex = answer("codex", true, true);
        codex.windows = Some(vec![("5h".into(), window(40.0, Some(18_000.0)))]);
        let mut claude = answer("claude", true, true);
        claude.windows = Some(vec![("7d".into(), window(20.0, Some(604_800.0)))]);
        let mut quota = quota(vec![("codex", Ok(codex)), ("claude", Ok(claude))]);
        quota
            .providers
            .get_mut("codex")
            .expect("codex")
            .apply(Err("connection refused".into()));
        let groups = quota.groups(&all(), &strings);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].provider_id, "claude");
        assert_eq!(groups[0].status, None, "the other provider is untouched");
        assert_eq!(groups[0].rows.len(), 1);
        assert_eq!(groups[1].rows.len(), 1, "the failed refresh keeps its row");
        assert_eq!(
            groups[1].status.as_deref(),
            Some("Showing previously received usage. Refresh failed: connection refused")
        );
        quota
            .providers
            .get_mut("codex")
            .expect("codex")
            .apply(Ok(answer("codex", true, false)));
        let groups = quota.groups(&all(), &strings);
        assert_eq!(
            groups[1].rows.len(),
            1,
            "the provider's own failure keeps the last sample too"
        );
        // An unconfigured answer was no sample: the failure replaces it.
        let mut fresh = ProviderState::default();
        fresh.apply(Ok(answer("codex", false, false)));
        fresh.apply(Ok(answer("codex", true, false)));
        assert!(
            fresh
                .result
                .as_ref()
                .is_some_and(|result| result.configured)
        );
        assert_eq!(fresh.error_line(&strings).as_deref(), Some("429"));
    }

    #[test]
    fn model_rows_follow_the_selection_and_show_their_first_window() {
        let strings = Strings::load("en");
        let mut google = answer("google", true, true);
        google.windows = Some(vec![("daily".into(), window(5.0, Some(86_400.0)))]);
        google.models = vec![
            (
                "gemini/pro".into(),
                vec![
                    ("weekly".into(), window(70.0, None)),
                    ("5h".into(), window(1.0, None)),
                ],
            ),
            (
                "gemini/flash".into(),
                vec![("5h".into(), window(2.0, None))],
            ),
            ("antigravity/claude".into(), Vec::new()),
            (
                "other/model".into(),
                vec![("daily".into(), window(3.0, None))],
            ),
        ];
        let quota = quota(vec![("google", Ok(google))]);
        let rows = |preferences: &UsagePreferences| -> Vec<(Option<String>, String, Option<f64>)> {
            quota.groups(preferences, &strings)[0]
                .rows
                .iter()
                .map(|row| {
                    (
                        row.subtitle.clone(),
                        row.label.clone(),
                        row.window.used_percent,
                    )
                })
                .collect()
        };
        assert_eq!(
            rows(&all()),
            [
                (None, "Daily".to_owned(), Some(5.0)),
                (Some("pro".to_owned()), "Weekly".to_owned(), Some(70.0)),
                (Some("flash".to_owned()), "5-Hour".to_owned(), Some(2.0)),
                (
                    Some("other/model".to_owned()),
                    "Daily".to_owned(),
                    Some(3.0)
                ),
            ],
            "provider windows, then each model's first window in the server's order"
        );
        let picked = UsagePreferences {
            models: vec![
                (
                    "google".into(),
                    vec!["other/model".into(), "gemini/flash".into()],
                ),
                ("codex".into(), vec!["gemini/pro".into()]),
            ],
            ..all()
        };
        assert_eq!(
            rows(&picked),
            [
                (None, "Daily".to_owned(), Some(5.0)),
                (Some("flash".to_owned()), "5-Hour".to_owned(), Some(2.0)),
                (
                    Some("other/model".to_owned()),
                    "Daily".to_owned(),
                    Some(3.0)
                ),
            ],
            "the full model name selects, and the server's order stays"
        );
        let emptied = UsagePreferences {
            models: vec![("google".into(), Vec::new())],
            ..all()
        };
        assert_eq!(
            rows(&emptied).len(),
            4,
            "no selected model shows every model"
        );
    }

    #[test]
    fn the_headline_is_the_shortest_window_of_the_composer_provider() {
        let row = |label: &str, subtitle: Option<&str>, seconds: Option<f64>| UsageRow {
            key: label.into(),
            label: label.into(),
            subtitle: subtitle.map(str::to_owned),
            window: window(10.0, seconds),
        };
        let group = |provider_id: &'static str, rows: Vec<UsageRow>| UsageGroup {
            provider_id,
            provider_name: provider_id,
            rows,
            status: None,
        };
        let groups = [
            group("codex", vec![row("Weekly Limit", None, Some(604_800.0))]),
            group(
                "opencode-go",
                vec![
                    row("Monthly Limit", None, Some(2_592_000.0)),
                    row("5-Hour", None, Some(18_000.0)),
                    row("Weekly Limit", None, Some(604_800.0)),
                ],
            ),
            group(
                "zai-coding-plan",
                vec![
                    row("5-Hour", Some("GLM-5"), Some(18_000.0)),
                    row("Weekly Limit", None, Some(604_800.0)),
                ],
            ),
            group(
                "claude",
                vec![
                    row("Credits Balance", None, None),
                    row("Weekly Limit", None, Some(604_800.0)),
                ],
            ),
            group("cursor", vec![row("Credits Balance", None, None)]),
            group("google", Vec::new()),
        ];
        let label =
            |provider: Option<&str>| headline(&groups, provider).map(|row| row.label.as_str());
        assert_eq!(label(Some("opencode-go")), Some("5-Hour"));
        assert_eq!(label(Some("  OpenAI ")), Some("Weekly Limit"));
        assert_eq!(label(Some("zai-coding-plan")), Some("Weekly Limit"));
        assert_eq!(label(Some("claude-code")), Some("Weekly Limit"));
        assert_eq!(label(Some("cursor")), Some("Credits Balance"));
        assert_eq!(label(Some("google")), None, "a provider without rows");
        assert_eq!(
            label(Some("mistral")),
            None,
            "another provider's quota is never shown"
        );
        assert_eq!(label(Some("")), None);
        assert_eq!(label(None), None);
        assert_eq!(quota_provider_id("kilo-code").as_deref(), Some("kilo"));
        assert_eq!(quota_provider_id("gemini").as_deref(), Some("google"));
        assert_eq!(quota_provider_id("opencode").as_deref(), Some("opencode"));
    }

    #[test]
    fn gift_resets_need_a_gift_and_a_known_window() {
        let gift = openchamber_native_transport::quota::GiftReset {
            record_id: 7.0,
            expire_at: 1.0,
        };
        let mut weekly = window(50.0, Some(604_800.0));
        assert_eq!(gift_reset_type(&weekly), None, "no gift");
        weekly.gift_reset = Some(gift);
        assert_eq!(gift_reset_type(&weekly), Some(GiftResetType::Week));
        let mut daily = window(50.0, Some(86_400.0));
        daily.gift_reset = Some(gift);
        assert_eq!(gift_reset_type(&daily), None, "no reset for this window");
    }
}
