//! The work status card's Usage section (`WorkStatusUsageSection.tsx`): the
//! selected quota providers that are configured, each as a quiet sub-heading
//! over one row per limit.
//!
//! The quotas belong to `usage_quota.rs`, shared with Settings' Usage page.
//! The section is absent while no selected provider is listed, so a failed
//! read never shows as "nothing configured": a provider whose first read
//! failed is listed with its failure, and one whose refresh failed keeps its
//! rows. Folded, the heading shows the tightest limit of the provider the
//! composer points at.

use std::time::SystemTime;

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::message_time::ClockPreference;
use openchamber_native_transport::quota::{
    GiftResetError, GiftResetType, UsageTone, UsageWindow, usage_tone,
};

use super::work_status_sections::Layout;
use super::{CAPTION_SIZE, HEADING_HEIGHT, ROW_HEIGHT, ROW_LINE, ROW_SIZE, XS_LINE, XS_SIZE};
use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, IconButton, glyph};
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::network::NetworkTask;
use crate::send_catalog::{Load, ModelControl};
use crate::settings_controls::ServerTarget;
use crate::sidebar_view;
use crate::theme::Palette;
use crate::ui_memory::{self, UiMemory};
use crate::usage_page::{reset_label, value_label};
use crate::usage_quota::{
    self, CardDemand, DisplayMode, GiftOutcome, UsageGroup, UsagePreferences, UsageRow, headline,
};
use crate::workspace::Workspace;

/// The section's key in `workStatusExpandedSections`.
const SECTION: &str = "usage";
/// A reset caption's 11 px text on the row's `leading-normal`.
const RESET_LINE: f32 = CAPTION_SIZE * 1.5;
/// `GIFT_RESET_URGENT_MS`: a gift this close to expiring turns to warning.
const GIFT_URGENT_MILLIS: f64 = 60.0 * 60.0 * 1000.0;
/// `GIFT_RESET_FRESH_REMAINING_PERCENT`: a limit this full needs no reset.
const GIFT_FRESH_REMAINING: f64 = 95.0;

/// `windowTone`: half used warns, four fifths used is an error.
fn window_tone(window: &UsageWindow, palette: &Palette) -> Option<Hsla> {
    match usage_tone(window.used_percent.filter(|used| used.is_finite())) {
        UsageTone::Critical => Some(palette.status_error),
        UsageTone::Warn => Some(palette.status_warning),
        UsageTone::Safe => None,
    }
}

/// The row's text: a model's limit names its model first.
fn row_text(row: &UsageRow) -> String {
    match &row.subtitle {
        Some(subtitle) => format!("{subtitle} \u{b7} {}", row.label),
        None => row.label.clone(),
    }
}

/// The metric a readout shows for `window`; `None` when it has none.
fn metric(window: &UsageWindow, mode: DisplayMode, strings: &Strings) -> Option<String> {
    Some(value_label(window, mode.percent(window), strings)).filter(|label| label != "-")
}

/// How a gift reset request ended, and the failure text to show under the
/// failure title. A request the server rejected itself changed nothing; any
/// other failure leaves open whether the gift was spent. The text is the
/// server's explanation, else the failure's own; a request that ended without
/// an answer has none, and the title stands alone.
fn gift_outcome(answer: Option<Result<(), GiftResetError>>) -> (GiftOutcome, Option<String>) {
    match answer {
        Some(Ok(())) => (GiftOutcome::Spent, None),
        Some(Err(error)) => {
            let outcome = if error.is_definitive() {
                GiftOutcome::Declined
            } else {
                GiftOutcome::Unknown
            };
            let text = error
                .message()
                .map_or_else(|| error.to_string(), str::to_owned);
            (outcome, Some(text))
        }
        None => (GiftOutcome::Unknown, None),
    }
}

/// Whether a gift reset is pointless now: 95 percent or more of the limit
/// remains, rounded as the percentage beside it is.
fn gift_not_needed(window: &UsageWindow) -> bool {
    window
        .remaining_percent
        .or(window.used_percent.map(|used| 100.0 - used))
        .is_some_and(|remaining| remaining.round() >= GIFT_FRESH_REMAINING)
}

impl Workspace {
    fn usage_preferences(&self) -> UsagePreferences {
        UsagePreferences::from_settings(self.send_catalog.settings.as_ref())
    }

    /// Whether a recovery, sign-in or compatibility screen stands in for the
    /// workspace, so nothing of it is on screen.
    pub(crate) fn usage_quota_gated(&self) -> bool {
        self.host_recovery.is_some()
            || self.managed_failure_visible()
            || self.connection_recovery().is_some()
            || self.authentication.visible
            || self
                .connection
                .as_ref()
                .is_some_and(|connection| connection.compatibility.blocking().is_some())
    }

    /// Tells the quotas which server is selected and whether the card's
    /// Usage section is mounted. Called while the workspace renders, with
    /// `card_mounted` as the card's own mounting decision; the quotas change
    /// only when one of these inputs did. The window's visibility reaches
    /// them from the platform's report, not from here.
    pub(crate) fn synchronize_usage_quota(&mut self, card_mounted: bool, cx: &mut Context<Self>) {
        let providers = self.usage_preferences().providers;
        // The saved selection decides what is read, so reads wait for it. A
        // settings read that failed leaves every provider selected.
        let settled = self.send_catalog.settings.is_some()
            || matches!(self.send_catalog.settings_load, Load::Failed);
        let section = card_mounted
            && Layout::for_workspace(self, cx)
                .visible_builtin_order()
                .any(|id| id == SECTION);
        let target = self.connection.as_ref().and_then(|connection| {
            Some(ServerTarget {
                runtime: self.runtime.clone(),
                generation: connection.generation,
                client: connection.client.clone()?,
                network: connection.network.handle(),
            })
        });
        let card = CardDemand {
            shown: section && settled,
            providers: providers.clone(),
        };
        if !self
            .usage_quota
            .read(cx)
            .matches(&self.runtime, target.as_ref(), &card)
        {
            let runtime = self.runtime.clone();
            self.usage_quota.update(cx, |quota, cx| {
                quota.synchronize(runtime, target, card, cx);
            });
        }
        if section {
            // `preloadProviderLogos` for the providers the section lists.
            let listed = self.usage_quota.read(cx).listed_providers(&providers);
            for provider in listed {
                self.ensure_provider_logo(provider, cx);
            }
        }
    }

    /// The heading's refresh: the selected providers again. A refresh in
    /// flight is the one that answers.
    fn refresh_usage(&mut self, cx: &mut Context<Self>) {
        let providers = self.usage_preferences().providers;
        self.usage_quota.update(cx, |quota, cx| {
            if !quota.is_loading() {
                quota.refresh(&providers, cx);
            }
        });
    }

    /// Spends a window's gift limit reset once, says how it went and reads
    /// the provider again (`UsageGiftResetButton`), unless the server rejected
    /// the request itself. A lost answer is a failure; the request is never
    /// repeated.
    fn use_gift_reset(
        &mut self,
        provider: &'static str,
        record_id: f64,
        reset: GiftResetType,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let quota = self.usage_quota.clone();
        let Some(ticket) = quota.update(cx, |quota, cx| quota.begin_gift(provider, record_id, cx))
        else {
            return;
        };
        let client = ticket.target.client.clone();
        let spend = NetworkTask::spawn_on(&ticket.target.network, async move {
            client.use_gift_reset(provider, record_id, reset).await
        });
        cx.spawn_in(window, async move |this, cx| {
            let (outcome, failure) = gift_outcome(spend.await);
            let _ = this.update_in(cx, |_, window, cx| {
                let strings = locale::strings(cx);
                let failed = strings.get("settings.usage.page.window.giftResetActivateFailed");
                let notification = match (outcome, failure) {
                    (GiftOutcome::Spent, _) => Notification::success(
                        strings.get("settings.usage.page.window.giftResetActivated"),
                    ),
                    (_, Some(text)) => Notification::error(SharedString::from(text)).title(failed),
                    (_, None) => Notification::error(failed),
                };
                window.push_notification(notification, cx);
                quota.update(cx, |quota, cx| quota.finish_gift(&ticket, outcome, cx));
            });
        })
        .detach();
    }

    /// The Usage section; `None` while no selected provider is listed.
    pub(crate) fn render_work_status_usage(
        &self,
        palette: &Palette,
        direction: Direction,
        first_heading: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let strings = locale::strings(cx);
        let preferences = self.usage_preferences();
        let quota = self.usage_quota.read(cx);
        let groups = quota.groups(&preferences, strings);
        if groups.is_empty() {
            return None;
        }
        let mode = quota.display_mode(preferences.mode);
        let loading = quota.is_loading();
        let clock = self
            .send_catalog
            .settings
            .as_ref()
            .map(|settings| settings.clock)
            .unwrap_or_default();
        // Folded until the user unfolds it, as `defaultExpanded` is off.
        let expanded = cx
            .try_global::<UiMemory>()
            .and_then(|memory| memory.work_status_section_expanded(SECTION))
            .unwrap_or(false);

        let composer_provider = match self.model_control() {
            ModelControl::Model { provider_id, .. } => Some(provider_id),
            _ => None,
        };
        let headline = headline(&groups, composer_provider.as_deref())
            .and_then(|row| Some((row, metric(&row.window, mode, strings)?)));
        let summary = div()
            .min_w_0()
            .max_w(gpui_kit::relative(0.6))
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .css_text(XS_SIZE, XS_LINE, cx)
            .text_color(palette.muted_foreground);
        let summary = match headline {
            Some((row, metric)) => summary
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(row.label.clone())),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .when_some(window_tone(&row.window, palette), |value, color| {
                            value.text_color(color)
                        })
                        .child(SharedString::from(metric)),
                ),
            // No quota of the composer's provider: the mode the rows show.
            None => summary.child(div().min_w_0().truncate().child(strings.get(match mode {
                DisplayMode::Usage => "header.services.used",
                DisplayMode::Remaining => "header.services.remaining",
            }))),
        };

        let foreground = palette.foreground;
        let title = strings.get("chat.workStatus.section.usage");
        let heading = on_activate(
            div()
                .id("work-status-usage")
                .role(Role::Button)
                .accessibility_id("work-status-usage")
                .aria_label(title.clone())
                .aria_expanded(expanded)
                .flex_1()
                .min_w_0()
                .h_full()
                .px(px(4.0))
                .rounded(px(CONTROL_RADIUS))
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .text_color(palette.muted_foreground)
                .hover(move |style| style.text_color(foreground))
                .child(glyph("timer", 14.0, palette.muted_foreground))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .css_text(XS_SIZE, XS_LINE, cx)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.foreground)
                        .child(title),
                )
                .child(sidebar_view::chevron(!expanded, direction, palette))
                .child(div().flex_1())
                .child(summary),
            cx,
            move |_, _, cx| {
                ui_memory::set_work_status_section_expanded(SECTION, !expanded, cx);
                cx.notify();
            },
        )
        .test_support();
        // A sibling of the heading's button, so refreshing never folds.
        let refresh_hint = strings.get("settings.usage.sidebar.actions.refreshTitle");
        let refresh = div()
            .id("work-status-usage-refresh-hint")
            .flex_shrink_0()
            .tooltip(move |window, cx| Tooltip::new(refresh_hint.clone()).build(window, cx))
            .child(
                IconButton::new(
                    "work-status-usage-refresh",
                    if loading { "loader-4" } else { "refresh" },
                    strings.get("settings.usage.sidebar.actions.refreshAria"),
                )
                .size(24.0, 14.0)
                .render(palette, cx, |this, _, cx| this.refresh_usage(cx)),
            );

        let mut rows = Vec::new();
        if expanded {
            for group in &groups {
                rows.push(self.render_usage_provider(group, palette, direction, cx));
                for row in &group.rows {
                    let pending = row
                        .window
                        .gift_reset
                        .is_some_and(|gift| quota.gift_pending(group.provider_id, gift.record_id));
                    rows.push(self.render_usage_limit(
                        group.provider_id,
                        row,
                        LimitView {
                            mode,
                            clock,
                            gift_pending: pending,
                        },
                        palette,
                        direction,
                        cx,
                    ));
                }
            }
        }
        Some(
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .h(px(HEADING_HEIGHT))
                        .mb(px(2.0))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(4.0))
                        .when(first_heading, |heading| {
                            heading.padding_end(direction, px(28.0))
                        })
                        .child(heading)
                        .child(refresh),
                )
                .children(rows)
                .into_any_element(),
        )
    }

    /// A provider's sub-heading: its logo and name. A provider without rows
    /// shows its status at the row's end; the name gives way to it down to a
    /// stub, and the whole status is in the row's tooltip.
    fn render_usage_provider(
        &self,
        group: &UsageGroup,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let status: Option<SharedString> = group
            .status
            .clone()
            .filter(|_| group.rows.is_empty())
            .map(SharedString::from);
        let id: SharedString = format!("work-status-usage-{}", group.provider_id).into();
        let label: SharedString = match &status {
            Some(status) => format!("{} {status}", group.provider_name).into(),
            None => group.provider_name.into(),
        };
        let name = div()
            .truncate()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(palette.foreground)
            .child(SharedString::from(group.provider_name));
        div()
            .id(id.clone())
            .role(Role::Group)
            .accessibility_id(id)
            .aria_label(label)
            .h(px(ROW_HEIGHT))
            .px(px(4.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .css_text(ROW_SIZE, ROW_LINE, cx)
            .text_color(palette.muted_foreground)
            .children(self.render_provider_logo(group.provider_id, 14.0, 1.0, palette))
            .map(|row| match status {
                Some(status) => {
                    let tooltip = status.clone();
                    row.child(name.flex_1().min_w(px(32.0)))
                        .child(div().min_w_0().truncate().child(status))
                        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                }
                None => row.child(name.flex_1().min_w_0()),
            })
            .test_support()
            .into_any_element()
    }

    /// One limit: its label with the reset beside it, and at the row's end
    /// the gift reset and the metric in the window's tone.
    fn render_usage_limit(
        &self,
        provider: &'static str,
        row: &UsageRow,
        view: LimitView,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let text = row_text(row);
        let metric = metric(&row.window, view.mode, strings);
        let reset = reset_label(&row.window, view.clock, cx);
        let id: SharedString = format!("work-status-usage-{provider}-{}", row.key).into();
        let label: SharedString = match &metric {
            Some(metric) => format!("{text} {metric}").into(),
            None => text.clone().into(),
        };
        let value = metric.map(|metric| {
            div()
                .flex_shrink_0()
                .flex_row_from(direction)
                .items_center()
                .gap(px(4.0))
                .children(self.render_gift_reset(provider, row, view, palette, cx))
                .child(
                    div()
                        .when_some(window_tone(&row.window, palette), |value, color| {
                            value.text_color(color)
                        })
                        .child(SharedString::from(metric)),
                )
        });
        div()
            .id(id.clone())
            .role(Role::Group)
            .accessibility_id(id)
            .aria_label(label)
            .h(px(ROW_HEIGHT))
            .px(px(4.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .css_text(ROW_SIZE, ROW_LINE, cx)
            .text_color(palette.muted_foreground)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex_row_from(direction)
                    .items_baseline()
                    .gap(px(6.0))
                    .child(div().min_w_0().truncate().child(SharedString::from(text)))
                    .children(reset.map(|reset| {
                        div()
                            .flex_shrink_0()
                            .css_text(CAPTION_SIZE, RESET_LINE, cx)
                            .child(SharedString::from(reset))
                    })),
            )
            .children(value)
            .test_support()
            .into_any_element()
    }

    /// `UsageGiftResetButton`: shown for a window that carries a gift and has
    /// a reset, in the success tint, or the warning tint in the gift's last
    /// hour. It does nothing while the gift is being spent or the limit is
    /// still nearly full.
    fn render_gift_reset(
        &self,
        provider: &'static str,
        row: &UsageRow,
        view: LimitView,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let gift = row.window.gift_reset?;
        let reset = usage_quota::gift_reset_type(&row.window)?;
        let strings = locale::strings(cx);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as f64)
            .unwrap_or_default();
        let color = if gift.expire_at - now < GIFT_URGENT_MILLIS {
            palette.status_warning
        } else {
            palette.status_success
        };
        let not_needed = gift_not_needed(&row.window);
        let not_needed_text = strings.get("settings.usage.page.window.giftResetNotNeeded");
        let aria = strings.get("settings.usage.page.window.giftResetAria");
        let label: SharedString = if not_needed {
            format!("{aria}. {not_needed_text}").into()
        } else {
            aria
        };
        let expires: String =
            crate::turn_footer::message_time(gift.expire_at as i64, view.clock, cx)
                .map_or_else(|| "-".to_owned(), |time| time.to_string());
        let expires = strings.format(
            "settings.usage.page.window.giftResetExpires",
            &[("time", &expires)],
        );
        let tooltip: SharedString = if not_needed {
            format!("{expires}.\n{not_needed_text}").into()
        } else {
            expires
        };
        let id: SharedString = format!("work-status-usage-gift-{provider}-{}", row.key).into();
        let button = div()
            .id(id.clone())
            .role(Role::Button)
            .accessibility_id(id)
            .aria_label(label)
            .size(px(24.0))
            .flex_shrink_0()
            .rounded(px(CONTROL_RADIUS))
            .flex()
            .items_center()
            .justify_center()
            .bg(color.opacity(0.15))
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .child(glyph(
                if view.gift_pending {
                    "loader-4"
                } else {
                    "gift"
                },
                14.0,
                color,
            ));
        if view.gift_pending || not_needed {
            return Some(button.opacity(0.5).test_support().into_any_element());
        }
        let record_id = gift.record_id;
        Some(
            on_activate(
                button
                    .cursor_pointer()
                    .hover(move |style| style.bg(color.opacity(0.25))),
                cx,
                move |this, window, cx| {
                    this.use_gift_reset(provider, record_id, reset, window, cx);
                },
            )
            .test_support()
            .into_any_element(),
        )
    }
}

/// How one limit row is drawn.
#[derive(Clone, Copy)]
struct LimitView {
    mode: DisplayMode,
    clock: ClockPreference,
    gift_pending: bool,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext, WindowVisibility};
    use openchamber_native_core::ids::RuntimeId;
    use openchamber_native_transport::client::{ServerClient, ServerSettings};
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::error::TransportError;
    use openchamber_native_transport::quota::QUOTA_PROVIDERS;
    use serde_json::{Value, json};

    use super::*;
    use crate::connection::{Connection, ServerConfig};
    use crate::settings_test_server::{self, Server};
    use crate::usage_quota::REFRESH_INTERVAL;

    /// A loopback server whose quota answers a test scripts per provider.
    /// A provider without a script answers that it is not configured, and a
    /// gift reset succeeds unless `gift` is scripted.
    struct Quotas {
        server: Server,
        answers: Arc<Mutex<HashMap<String, (u16, Value)>>>,
    }

    impl Quotas {
        fn new() -> Self {
            let answers: Arc<Mutex<HashMap<String, (u16, Value)>>> = Arc::default();
            let scripted = Arc::clone(&answers);
            let server = Server::new(move |request| {
                let path = request.target.split('?').next().unwrap_or_default();
                if request.method == "POST" && path.ends_with("/gift-reset/use") {
                    return scripted
                        .lock()
                        .expect("answers")
                        .get("gift")
                        .cloned()
                        .unwrap_or_else(|| (200, json!({"success": true})));
                }
                let Some(provider) = path.strip_prefix("/api/quota/") else {
                    return (200, json!({}));
                };
                scripted
                    .lock()
                    .expect("answers")
                    .get(provider)
                    .cloned()
                    .unwrap_or_else(|| (200, answer(provider, false, None)))
            });
            Self { server, answers }
        }

        fn script(&self, provider: &str, status: u16, body: Value) {
            self.answers
                .lock()
                .expect("answers")
                .insert(provider.to_owned(), (status, body));
        }

        /// The providers whose quota was read, in request order.
        fn reads(&self) -> Vec<String> {
            self.server
                .requests()
                .iter()
                .filter(|request| request.method == "GET")
                .filter_map(|request| request.target.strip_prefix("/api/quota/"))
                .map(str::to_owned)
                .collect()
        }
    }

    fn quota_window(used: f64, seconds: f64) -> Value {
        json!({
            "usedPercent": used, "remainingPercent": 100.0 - used, "windowSeconds": seconds,
            "resetAfterSeconds": null, "resetAt": null, "resetAtFormatted": null,
            "resetAfterFormatted": "2h",
        })
    }

    /// A provider's answer; `usage` of a configured provider is its windows.
    fn answer(provider: &str, configured: bool, usage: Option<Value>) -> Value {
        json!({
            "providerId": provider, "providerName": provider, "ok": configured,
            "configured": configured, "usage": usage, "fetchedAt": 1,
        })
    }

    fn selecting(providers: &[&str]) -> ServerSettings {
        ServerSettings {
            usage_providers: Some(providers.iter().map(|id| (*id).to_owned()).collect()),
            ..ServerSettings::default()
        }
    }

    /// A workspace connected to `quotas` with `settings` read, its card shown.
    fn open(
        quotas: &Quotas,
        settings: ServerSettings,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        connect(&workspace, quotas, settings, cx);
        settings_test_server::settle(window, cx);
        (window, workspace)
    }

    fn connect(
        workspace: &Entity<Workspace>,
        quotas: &Quotas,
        settings: ServerSettings,
        cx: &mut TestAppContext,
    ) {
        connect_generation(workspace, quotas, settings, 0, cx);
    }

    /// Connects as the `generation`th attempt at the workspace's runtime.
    fn connect_generation(
        workspace: &Entity<Workspace>,
        quotas: &Quotas,
        settings: ServerSettings,
        generation: u64,
        cx: &mut TestAppContext,
    ) {
        let endpoint = quotas.server.endpoint.clone();
        workspace.update(cx, |workspace, cx| {
            let mut connection =
                Connection::new(ServerConfig::for_endpoint(endpoint.clone())).expect("network");
            connection.client =
                Some(ServerClient::new(endpoint, Credential::None).expect("fixture client"));
            connection.generation = generation;
            workspace.connection = Some(connection);
            workspace.send_catalog.settings = Some(settings);
            cx.notify();
        });
    }

    fn group_ids(workspace: &Entity<Workspace>, cx: &TestAppContext) -> Vec<&'static str> {
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            workspace
                .usage_quota
                .read(cx)
                .groups(&workspace.usage_preferences(), locale::strings(cx))
                .iter()
                .map(|group| group.provider_id)
                .collect()
        })
    }

    fn shown(window: AnyWindowHandle, id: &'static str, cx: &mut TestAppContext) -> bool {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .expect("test window")
    }

    fn click(window: AnyWindowHandle, id: &'static str, cx: &mut TestAppContext) {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .expect("test window");
        settings_test_server::settle(window, cx);
    }

    #[gpui_kit::test]
    fn populated_quotas_read_the_selection_once_and_unfold_into_rows(cx: &mut TestAppContext) {
        let quotas = Quotas::new();
        quotas.script(
            "codex",
            200,
            answer(
                "codex",
                true,
                Some(json!({
                    "windows": {"5h": quota_window(82.0, 18_000.0), "7d": quota_window(40.0, 604_800.0)},
                    "models": {"gpt": {"windows": {"5h": quota_window(10.0, 18_000.0)}}},
                })),
            ),
        );
        let (window, workspace) = open(&quotas, selecting(&["codex", "claude"]), cx);
        let mut reads = quotas.reads();
        reads.sort();
        assert_eq!(
            reads,
            ["claude", "codex"],
            "only the selected providers are read, each once"
        );
        assert_eq!(
            group_ids(&workspace, cx),
            ["codex"],
            "the unconfigured provider is no readout"
        );
        assert!(shown(window, "work-status-usage", cx));
        assert!(
            !shown(window, "work-status-usage-codex", cx),
            "the section starts folded"
        );
        click(window, "work-status-usage", cx);
        for row in [
            "work-status-usage-codex",
            "work-status-usage-codex-window-5h",
            "work-status-usage-codex-window-7d",
            "work-status-usage-codex-model-gpt-5h",
        ] {
            assert!(shown(window, row, cx), "{row} is drawn");
        }
        assert_eq!(
            cx.read(|cx| cx
                .global::<UiMemory>()
                .work_status_section_expanded(SECTION)),
            Some(true),
            "the unfolded section is remembered"
        );
        // Rendering again reads nothing more.
        settings_test_server::settle(window, cx);
        assert_eq!(quotas.reads().len(), 2);
        // Three minutes on, the shown section reads its selection again.
        cx.executor().advance_clock(REFRESH_INTERVAL);
        settings_test_server::settle(window, cx);
        assert_eq!(
            quotas.reads().len(),
            4,
            "one timed read per selected provider"
        );
    }

    #[gpui_kit::test]
    fn nothing_configured_shows_no_section_and_stops_reading(cx: &mut TestAppContext) {
        let quotas = Quotas::new();
        let (window, workspace) = open(&quotas, selecting(&["codex", "claude"]), cx);
        assert_eq!(quotas.reads().len(), 2);
        assert!(group_ids(&workspace, cx).is_empty());
        assert!(!shown(window, "work-status-usage", cx));
        cx.executor().advance_clock(REFRESH_INTERVAL);
        settings_test_server::settle(window, cx);
        assert_eq!(
            quotas.reads().len(),
            2,
            "a confirmed absence asks for no further reads"
        );
    }

    #[gpui_kit::test]
    fn a_saved_empty_selection_reads_nothing(cx: &mut TestAppContext) {
        let quotas = Quotas::new();
        let (window, workspace) = open(&quotas, selecting(&[]), cx);
        assert!(quotas.reads().is_empty());
        assert!(group_ids(&workspace, cx).is_empty());
        assert!(!shown(window, "work-status-usage", cx));
    }

    #[gpui_kit::test]
    fn the_section_waits_for_the_first_answer_without_showing_an_empty_one(
        cx: &mut TestAppContext,
    ) {
        let quotas = Quotas::new();
        quotas.script(
            "codex",
            200,
            answer(
                "codex",
                true,
                Some(json!({"windows": {"5h": quota_window(10.0, 18_000.0)}})),
            ),
        );
        let (window, workspace) = crate::workspace_tests::open(cx);
        connect(&workspace, &quotas, selecting(&["codex"]), cx);
        // One frame starts the read; its answer has not been taken yet.
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .expect("test window");
        let loading = cx.read(|cx| workspace.read(cx).usage_quota.read(cx).is_loading());
        assert!(loading, "the first read is in flight");
        assert!(group_ids(&workspace, cx).is_empty());
        assert!(
            !shown(window, "work-status-usage", cx),
            "no readout is drawn before a provider answered"
        );
        settings_test_server::settle(window, cx);
        assert_eq!(group_ids(&workspace, cx), ["codex"]);
        assert!(shown(window, "work-status-usage", cx));
        let loading = cx.read(|cx| workspace.read(cx).usage_quota.read(cx).is_loading());
        assert!(!loading);
    }

    #[gpui_kit::test]
    fn a_failed_first_read_is_listed_as_a_failure_and_read_again(cx: &mut TestAppContext) {
        let quotas = Quotas::new();
        quotas.script("codex", 500, json!({"error": "boom"}));
        let (window, workspace) = open(&quotas, selecting(&["codex"]), cx);
        assert_eq!(
            group_ids(&workspace, cx),
            ["codex"],
            "an unanswered provider is not taken for an unconfigured one"
        );
        let status = cx.read(|cx| {
            let workspace = workspace.read(cx);
            workspace
                .usage_quota
                .read(cx)
                .groups(&workspace.usage_preferences(), locale::strings(cx))[0]
                .status
                .clone()
        });
        assert!(status.is_some(), "the failure is the provider's status");
        assert!(shown(window, "work-status-usage", cx));
        // The runtime never answered, so the next timed read asks again.
        quotas.script(
            "codex",
            200,
            answer(
                "codex",
                true,
                Some(json!({"windows": {"5h": quota_window(10.0, 18_000.0)}})),
            ),
        );
        cx.executor().advance_clock(REFRESH_INTERVAL);
        settings_test_server::settle(window, cx);
        assert_eq!(quotas.reads(), ["codex", "codex"]);
        let rows = cx.read(|cx| {
            let workspace = workspace.read(cx);
            workspace
                .usage_quota
                .read(cx)
                .groups(&workspace.usage_preferences(), locale::strings(cx))[0]
                .rows
                .len()
        });
        assert_eq!(rows, 1);
    }

    #[gpui_kit::test]
    fn a_failed_refresh_keeps_rows_and_one_provider_failing_leaves_the_other(
        cx: &mut TestAppContext,
    ) {
        let quotas = Quotas::new();
        for provider in ["codex", "claude"] {
            quotas.script(
                provider,
                200,
                answer(
                    provider,
                    true,
                    Some(json!({"windows": {"5h": quota_window(10.0, 18_000.0)}})),
                ),
            );
        }
        let (window, workspace) = open(&quotas, selecting(&["codex", "claude"]), cx);
        assert_eq!(group_ids(&workspace, cx), ["claude", "codex"]);
        quotas.script("codex", 503, json!({"error": "unavailable"}));
        quotas.script(
            "claude",
            200,
            answer(
                "claude",
                true,
                Some(json!({"windows": {"5h": quota_window(60.0, 18_000.0)}})),
            ),
        );
        // The heading's refresh, a sibling of the fold toggle.
        click(window, "work-status-usage-refresh", cx);
        assert_eq!(
            cx.read(|cx| cx
                .try_global::<UiMemory>()
                .and_then(|memory| memory.work_status_section_expanded(SECTION))),
            None,
            "refreshing does not unfold the section"
        );
        let groups = cx.read(|cx| {
            let workspace = workspace.read(cx);
            workspace
                .usage_quota
                .read(cx)
                .groups(&workspace.usage_preferences(), locale::strings(cx))
        });
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].provider_id, "claude");
        assert_eq!(groups[0].rows[0].window.used_percent, Some(60.0));
        assert_eq!(groups[0].status, None);
        assert_eq!(groups[1].provider_id, "codex");
        assert_eq!(
            groups[1].rows[0].window.used_percent,
            Some(10.0),
            "the failed refresh keeps the last sample"
        );
        assert!(
            groups[1]
                .status
                .as_deref()
                .is_some_and(|status| status.starts_with("Showing previously received usage.")),
            "and says the sample is stale"
        );
        assert_eq!(quotas.reads().len(), 4);
    }

    #[gpui_kit::test]
    fn another_runtime_starts_empty_and_discards_reads_of_the_one_left(cx: &mut TestAppContext) {
        let first = Quotas::new();
        first.script(
            "codex",
            200,
            answer(
                "codex",
                true,
                Some(json!({"windows": {"5h": quota_window(10.0, 18_000.0)}})),
            ),
        );
        let (window, workspace) = open(&first, selecting(&["codex"]), cx);
        assert_eq!(group_ids(&workspace, cx), ["codex"]);
        // A refresh of the first runtime is in flight when the runtime changes.
        workspace.update(cx, |workspace, cx| workspace.refresh_usage(cx));
        let second = Quotas::new();
        workspace.update(cx, |workspace, _| {
            workspace.runtime = RuntimeId::from("another");
        });
        connect(&workspace, &second, selecting(&["codex"]), cx);
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .expect("test window");
        assert!(
            group_ids(&workspace, cx).is_empty(),
            "the first runtime's sample is not shown as the second's"
        );
        settings_test_server::settle(window, cx);
        assert!(
            group_ids(&workspace, cx).is_empty(),
            "the retired read of the first runtime lands nowhere"
        );
        assert_eq!(
            second.reads(),
            ["codex"],
            "the second runtime is asked itself"
        );
        assert!(!shown(window, "work-status-usage", cx));
    }

    #[gpui_kit::test]
    fn model_rows_follow_the_saved_selection_and_the_registry_orders_providers(
        cx: &mut TestAppContext,
    ) {
        let quotas = Quotas::new();
        quotas.script(
            "google",
            200,
            answer(
                "google",
                true,
                Some(json!({
                    "windows": {"daily": quota_window(5.0, 86_400.0)},
                    "models": {
                        "gemini/flash": {"windows": {"5h": quota_window(2.0, 18_000.0)}},
                        "gemini/pro": {"windows": {"5h": quota_window(70.0, 18_000.0)}},
                    },
                })),
            ),
        );
        quotas.script(
            "claude",
            200,
            answer(
                "claude",
                true,
                Some(json!({"windows": {"7d": quota_window(20.0, 604_800.0)}})),
            ),
        );
        let settings = ServerSettings {
            usage_selected_models: vec![("google".into(), vec!["gemini/pro".into()])],
            ..selecting(&["google", "claude"])
        };
        let (_window, workspace) = open(&quotas, settings, cx);
        let rows: Vec<(&'static str, Vec<String>)> = cx.read(|cx| {
            let workspace = workspace.read(cx);
            workspace
                .usage_quota
                .read(cx)
                .groups(&workspace.usage_preferences(), locale::strings(cx))
                .iter()
                .map(|group| (group.provider_id, group.rows.iter().map(row_text).collect()))
                .collect()
        });
        assert_eq!(
            rows,
            [
                ("claude", vec!["7-Day Limit".to_owned()]),
                (
                    "google",
                    vec!["Daily".to_owned(), "pro \u{b7} 5-Hour".to_owned()]
                ),
            ],
            "the registry's provider order; only the selected model's row"
        );
    }

    #[gpui_kit::test]
    fn a_hidden_window_stops_timed_reads_and_showing_it_reads_at_once(cx: &mut TestAppContext) {
        let quotas = Quotas::new();
        quotas.script(
            "codex",
            200,
            answer(
                "codex",
                true,
                Some(json!({"windows": {"5h": quota_window(10.0, 18_000.0)}})),
            ),
        );
        let (window, _workspace) = open(&quotas, selecting(&["codex"]), cx);
        assert_eq!(quotas.reads().len(), 1);
        cx.simulate_window_visibility_change(window, WindowVisibility::Hidden);
        cx.executor().advance_clock(REFRESH_INTERVAL);
        settings_test_server::settle(window, cx);
        assert_eq!(
            quotas.reads().len(),
            1,
            "a hidden window reads nothing on the interval"
        );
        // Presented again, long after the last read: no wait for the interval.
        cx.simulate_window_visibility_change(window, WindowVisibility::Visible);
        settings_test_server::settle(window, cx);
        assert_eq!(quotas.reads().len(), 2, "showing the window reads at once");
        cx.simulate_window_visibility_change(window, WindowVisibility::Hidden);
        cx.simulate_window_visibility_change(window, WindowVisibility::Visible);
        settings_test_server::settle(window, cx);
        assert_eq!(quotas.reads().len(), 2, "a read this fresh is not repeated");
        cx.executor().advance_clock(REFRESH_INTERVAL);
        settings_test_server::settle(window, cx);
        assert_eq!(
            quotas.reads().len(),
            3,
            "the interval runs again while shown"
        );
    }

    #[gpui_kit::test]
    fn a_new_connection_reads_every_provider_again_for_the_usage_page(cx: &mut TestAppContext) {
        let first = Quotas::new();
        let (window, workspace) = open(&first, selecting(&[]), cx);
        // The Usage page asks for every provider, and the connection is
        // replaced while those reads are in flight.
        workspace.update(cx, |workspace, cx| {
            workspace
                .usage_quota
                .update(cx, |quota, cx| quota.ensure_all(cx));
        });
        let loading = cx.read(|cx| workspace.read(cx).usage_quota.read(cx).is_loading());
        assert!(loading);
        let second = Quotas::new();
        connect_generation(&workspace, &second, selecting(&[]), 1, cx);
        settings_test_server::settle(window, cx);
        settings_test_server::settle(window, cx);
        let mut reads = second.reads();
        reads.sort();
        reads.dedup();
        assert_eq!(
            reads.len(),
            QUOTA_PROVIDERS.len(),
            "the new connection is asked for every provider"
        );
        assert_eq!(second.reads().len(), QUOTA_PROVIDERS.len(), "each once");
        let (answered, loading) = cx.read(|cx| {
            let quota = workspace.read(cx).usage_quota.read(cx);
            (quota.provider_count(), quota.is_loading())
        });
        assert_eq!(answered, QUOTA_PROVIDERS.len());
        assert!(!loading);
    }

    #[gpui_kit::test]
    fn a_provider_added_to_the_selection_is_read_without_waiting(cx: &mut TestAppContext) {
        let quotas = Quotas::new();
        quotas.script(
            "claude",
            200,
            answer(
                "claude",
                true,
                Some(json!({"windows": {"5h": quota_window(10.0, 18_000.0)}})),
            ),
        );
        let (window, workspace) = open(&quotas, selecting(&["codex"]), cx);
        assert_eq!(quotas.reads(), ["codex"]);
        assert!(
            group_ids(&workspace, cx).is_empty(),
            "the runtime answered and nothing selected is configured"
        );
        workspace.update(cx, |workspace, cx| {
            workspace.send_catalog.settings = Some(selecting(&["codex", "claude"]));
            cx.notify();
        });
        settings_test_server::settle(window, cx);
        assert_eq!(
            quotas.reads(),
            ["codex", "claude"],
            "only the added provider is read, once"
        );
        assert_eq!(group_ids(&workspace, cx), ["claude"]);
        assert!(shown(window, "work-status-usage", cx));
    }

    #[gpui_kit::test]
    fn a_settled_gift_reads_its_provider_in_place_of_an_earlier_read(cx: &mut TestAppContext) {
        const PROVIDER: &str = "zai-coding-plan";
        let quotas = Quotas::new();
        quotas.script(
            PROVIDER,
            200,
            answer(
                PROVIDER,
                true,
                Some(json!({"windows": {"5h": quota_window(50.0, 18_000.0)}})),
            ),
        );
        let (window, workspace) = open(&quotas, selecting(&[PROVIDER]), cx);
        let quota = cx.read(|cx| workspace.read(cx).usage_quota.clone());
        for outcome in [GiftOutcome::Spent, GiftOutcome::Unknown] {
            let (earlier, later) = quota.update(cx, |quota, cx| {
                let ticket = quota.begin_gift(PROVIDER, 7.0, cx).expect("accepted");
                assert!(
                    quota.begin_gift(PROVIDER, 7.0, cx).is_none(),
                    "the same gift is spent once at a time"
                );
                // A read asked before the gift settled is still in flight.
                quota.refresh(&[PROVIDER], cx);
                let earlier = quota.read_in_flight(PROVIDER);
                quota.finish_gift(&ticket, outcome, cx);
                (earlier, quota.read_in_flight(PROVIDER))
            });
            assert!(earlier.is_some());
            assert!(
                later > earlier,
                "{outcome:?}: a new read replaces the one asked before the gift settled"
            );
            settings_test_server::settle(window, cx);
            let (loading, pending) = quota.read_with(cx, |quota, _| {
                (quota.is_loading(), quota.gift_pending(PROVIDER, 7.0))
            });
            assert!(!loading, "the replacing read answers");
            assert!(!pending);
        }
        let read = quota.update(cx, |quota, cx| {
            let ticket = quota.begin_gift(PROVIDER, 7.0, cx).expect("accepted");
            quota.finish_gift(&ticket, GiftOutcome::Declined, cx);
            quota.read_in_flight(PROVIDER)
        });
        assert_eq!(read, None, "a rejected request changed nothing to read");
    }

    #[gpui_kit::test]
    fn the_gift_button_follows_the_servers_answer(cx: &mut TestAppContext) {
        const PROVIDER: &str = "zai-coding-plan";
        const GIFT: &str = "work-status-usage-gift-zai-coding-plan-window-5h";
        let quotas = Quotas::new();
        let mut gifted = quota_window(50.0, 18_000.0);
        gifted["giftReset"] = json!({"recordId": 7, "expireAt": 4_102_444_800_000_u64});
        quotas.script(
            PROVIDER,
            200,
            answer(PROVIDER, true, Some(json!({"windows": {"5h": gifted}}))),
        );
        let (window, workspace) = open(&quotas, selecting(&[PROVIDER]), cx);
        click(window, "work-status-usage", cx);
        assert!(shown(window, GIFT, cx));
        assert_eq!(quotas.reads().len(), 1);
        let pending = |cx: &mut TestAppContext| {
            cx.read(|cx| {
                workspace
                    .read(cx)
                    .usage_quota
                    .read(cx)
                    .gift_pending(PROVIDER, 7.0)
            })
        };

        click(window, GIFT, cx);
        let posts = quotas.server.writes();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].target, "/api/quota/zai-coding-plan/gift-reset/use");
        assert_eq!(
            posts[0].body,
            json!({"recordId": 7.0, "resetType": "FIVE_HOUR"})
        );
        assert_eq!(
            quotas.reads().len(),
            2,
            "a spent gift reads its provider again"
        );
        assert!(!pending(cx));

        quotas.script("gift", 400, json!({"error": "Invalid gift reset request"}));
        click(window, GIFT, cx);
        assert_eq!(quotas.server.writes().len(), 2);
        assert_eq!(quotas.reads().len(), 2, "a rejected request reads nothing");
        assert!(!pending(cx));

        quotas.script("gift", 502, json!({"error": "provider failed"}));
        click(window, GIFT, cx);
        assert_eq!(quotas.server.writes().len(), 3, "the request is sent once");
        assert_eq!(
            quotas.reads().len(),
            3,
            "a relayed provider failure reads again"
        );

        quotas.script("gift", 200, json!({"success": false}));
        click(window, GIFT, cx);
        assert_eq!(quotas.server.writes().len(), 4);
        assert_eq!(
            quotas.reads().len(),
            4,
            "an unconfirmed receipt reads again"
        );
        assert!(!pending(cx));
    }

    #[test]
    fn a_gift_failure_names_its_cause_and_never_shows_an_empty_text() {
        let refused = |status: u16, message: Option<&str>| {
            Some(Err(GiftResetError::Refused {
                status,
                message: message.map(str::to_owned),
            }))
        };
        assert_eq!(gift_outcome(Some(Ok(()))), (GiftOutcome::Spent, None));
        assert_eq!(
            gift_outcome(refused(400, Some("Not configured"))),
            (GiftOutcome::Declined, Some("Not configured".to_owned())),
            "the server's own explanation is shown"
        );
        assert_eq!(
            gift_outcome(refused(502, Some("provider failed"))),
            (GiftOutcome::Unknown, Some("provider failed".to_owned())),
            "a relayed provider failure may have spent the gift"
        );
        let (outcome, text) = gift_outcome(refused(404, None));
        assert_eq!(outcome, GiftOutcome::Declined);
        assert!(text.is_some_and(|text| !text.is_empty()));
        let (outcome, text) = gift_outcome(Some(Err(TransportError::TimedOut.into())));
        assert_eq!(outcome, GiftOutcome::Unknown, "a timeout says nothing");
        assert!(text.is_some_and(|text| !text.is_empty()));
        assert_eq!(
            gift_outcome(None),
            (GiftOutcome::Unknown, None),
            "a request that ended without an answer shows the title alone"
        );
    }

    #[test]
    fn metrics_tones_and_gift_resets_follow_the_reference_thresholds() {
        let strings = Strings::load("en");
        let palette = Palette::default_for(crate::theme::Appearance::Dark);
        let used = |used: f64| UsageWindow {
            used_percent: Some(used),
            remaining_percent: Some(100.0 - used),
            ..UsageWindow::default()
        };
        assert_eq!(window_tone(&used(49.9), &palette), None);
        assert_eq!(
            window_tone(&used(50.0), &palette),
            Some(palette.status_warning)
        );
        assert_eq!(
            window_tone(&used(80.0), &palette),
            Some(palette.status_error)
        );
        assert_eq!(window_tone(&UsageWindow::default(), &palette), None);
        assert_eq!(
            metric(&used(42.4), DisplayMode::Usage, &strings).as_deref(),
            Some("42%")
        );
        assert_eq!(
            metric(&used(42.4), DisplayMode::Remaining, &strings).as_deref(),
            Some("58%")
        );
        assert_eq!(
            metric(&UsageWindow::default(), DisplayMode::Usage, &strings),
            None,
            "a window without a value shows none"
        );
        assert!(gift_not_needed(&used(5.4)), "94.6 percent rounds to 95");
        assert!(!gift_not_needed(&used(5.6)));
        let used_only = UsageWindow {
            used_percent: Some(2.0),
            ..UsageWindow::default()
        };
        assert!(
            gift_not_needed(&used_only),
            "what remains follows from what was used"
        );
        assert!(!gift_not_needed(&UsageWindow::default()));
        let model = UsageRow {
            key: "model-gpt-5h".into(),
            label: "5-Hour".into(),
            subtitle: Some("gpt".into()),
            window: used(1.0),
        };
        assert_eq!(row_text(&model), "gpt \u{b7} 5-Hour");
    }
}
