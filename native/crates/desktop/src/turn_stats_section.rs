//! The work status card's Turn stats section
//! (`WorkStatusTelemetrySection.tsx`): the latest completed turn's speed,
//! durations, tokens, cache use and cost, read from the transcript.
//!
//! The statistics are recomputed while the session is idle; while a turn
//! runs, the section keeps the last ones it committed, as the web's record
//! subscription pauses while the session works.

use gpui_kit::{AnyElement, Context, SharedString};
use openchamber_native_core::turn_stats::{self, TurnStats};

use crate::context_panel::{Section, collapsible_section, work_status_row};
use crate::locale::{self, Direction};
use crate::theme::Palette;
use crate::workspace::Workspace;

/// A cache share at or above this reads as healthy.
const CACHE_GOOD_PERCENT: u64 = 50;

impl Workspace {
    /// The section, for a selected session; its heading stays without
    /// statistics.
    pub(crate) fn render_turn_stats(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let id = self.selected.as_ref()?;
        let open = self.open_sessions.get(id)?;
        let stats = if self.is_streaming(id) {
            open.turn_stats.get()
        } else {
            let stats = turn_stats::latest_completed_turn(open.transcript.messages());
            open.turn_stats.set(stats);
            stats
        };
        let strings = locale::strings(cx);
        let rows = stats
            .map(|stats| self.turn_stat_rows(&stats, palette, direction, cx))
            .unwrap_or_default();
        Some(collapsible_section(
            Section {
                id: "work-status-telemetry",
                icon: "bar-chart-2",
                title: strings.get("chat.workStatus.section.telemetry"),
                summary: stats
                    .and_then(|stats| stats.response_tokens_per_second)
                    .map(|rate| turn_stats::format_rate(rate).into()),
                expanded: self.work_status_telemetry_open,
                toggle: |this| this.work_status_telemetry_open = !this.work_status_telemetry_open,
            },
            rows,
            palette,
            direction,
            cx,
        ))
    }

    /// Every reading the turn has, in the web's order.
    fn turn_stat_rows(
        &self,
        stats: &TurnStats,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let strings = locale::strings(cx);
        let duration = |ms: i64| SharedString::from(turn_stats::format_duration(ms as f64));
        let tokens = match (stats.input_tokens, stats.generated_tokens) {
            (Some(input), Some(generated)) if stats.reasoning_tokens.is_some() => {
                Some(strings.format(
                    "chat.workStatus.telemetry.tokens.inOut",
                    &[
                        ("input", &turn_stats::format_tokens(input)),
                        ("output", &turn_stats::format_tokens(generated)),
                    ],
                ))
            }
            _ => None,
        };
        let readings: [(&str, Option<SharedString>); 10] = [
            (
                "chat.workStatus.telemetry.responseSpeed",
                stats
                    .response_tokens_per_second
                    .map(|rate| turn_stats::format_rate(rate).into()),
            ),
            (
                "chat.workStatus.telemetry.speed",
                stats
                    .tokens_per_second
                    .map(|rate| turn_stats::format_rate(rate).into()),
            ),
            (
                "chat.workStatus.telemetry.elapsed",
                stats.elapsed_ms.map(duration),
            ),
            (
                "chat.workStatus.telemetry.llmDuration",
                stats.model_ms.map(duration),
            ),
            (
                "chat.workStatus.telemetry.toolDuration",
                stats.tool_ms.map(duration),
            ),
            (
                "chat.workStatus.telemetry.ttft",
                stats
                    .average_ttft_ms
                    .map(|ms| turn_stats::format_duration(ms).into()),
            ),
            (
                "chat.workStatus.telemetry.steps",
                (stats.steps > 1).then(|| stats.steps.to_string().into()),
            ),
            ("chat.workStatus.telemetry.tokens", tokens),
            (
                "chat.workStatus.telemetry.cacheHit",
                stats
                    .cache_hit_percent
                    .map(|percent| format!("{percent}%").into()),
            ),
            (
                "chat.workStatus.telemetry.cost",
                stats.cost.map(|cost| turn_stats::format_cost(cost).into()),
            ),
        ];
        let cache_good = stats
            .cache_hit_percent
            .is_some_and(|percent| percent >= CACHE_GOOD_PERCENT);
        readings
            .into_iter()
            .filter_map(|(key, value)| {
                let tone = (key == "chat.workStatus.telemetry.cacheHit" && cache_good)
                    .then_some(palette.status_success);
                value.map(|value| {
                    work_status_row(strings.get(key), value, tone, palette, direction, cx)
                })
            })
            .collect()
    }
}
