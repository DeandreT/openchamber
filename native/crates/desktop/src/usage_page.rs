//! Settings' Usage page (`UsagePage.tsx`, `UsageSidebar.tsx`): every quota
//! provider in a list with its fullest window's tone, and the selected
//! provider's windows and per-model windows.
//!
//! Each provider is read on its own (`GET /api/quota/:providerId`), so one
//! failing provider leaves the others' answers. A refresh that fails keeps
//! the last answer on screen and says so, as `useQuotaStore` does. Provider
//! logos, credential forms, Claude Code sign-in, gift resets, console
//! budgets, the work status toggle and model family grouping are not ported.

use std::collections::HashMap;
use std::time::SystemTime;

use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Render, Role,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Task, TestSupportExt,
    WeakEntity, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_core::message_time::ClockPreference;
use openchamber_native_transport::quota::{
    QUOTA_PROVIDERS, QuotaResult, UsageTone, UsageWindow, WindowValue, usage_tone,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::IconButton;
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::network::NetworkTask;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// Whether bars show the used or the remaining share (`usageDisplayMode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DisplayMode {
    #[default]
    Usage,
    Remaining,
}

/// One provider's last answer and its newest refresh failure.
#[derive(Clone, Debug, Default)]
pub(crate) struct ProviderState {
    pub(crate) result: Option<QuotaResult>,
    pub(crate) refresh_error: Option<String>,
}

impl ProviderState {
    /// `fetchProviderQuota`: a failed provider answer keeps the last sample.
    pub(crate) fn apply(&mut self, answer: Result<QuotaResult, String>) {
        match answer {
            Ok(result) if !result.ok && result.configured => {
                self.refresh_error = Some(
                    result
                        .error
                        .clone()
                        .unwrap_or_else(|| "Failed to fetch quota".into()),
                );
                match &mut self.result {
                    Some(previous) => previous.configured = true,
                    None => self.result = Some(result),
                }
            }
            Ok(result) => {
                self.refresh_error = None;
                self.result = Some(result);
            }
            Err(error) => self.refresh_error = Some(error),
        }
    }

    /// The panel's error line: stale usage says it is stale.
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
}

#[derive(Default)]
struct RuntimeUsage {
    providers: HashMap<&'static str, ProviderState>,
    pending: usize,
    last_updated: Option<i64>,
    tasks: Vec<Task<()>>,
}

pub(crate) struct UsagePage {
    owner: WeakEntity<Workspace>,
    runtimes: HashMap<RuntimeId, RuntimeUsage>,
    selected: Option<&'static str>,
    mode: DisplayMode,
    scroll: ScrollHandle,
}

impl UsagePage {
    pub(crate) fn new(owner: WeakEntity<Workspace>) -> Self {
        Self {
            owner,
            runtimes: HashMap::new(),
            selected: None,
            mode: DisplayMode::default(),
            scroll: ScrollHandle::new(),
        }
    }

    fn runtime(&self, cx: &gpui_kit::App) -> Option<RuntimeId> {
        Some(self.owner.upgrade()?.read(cx).runtime.clone())
    }

    fn clock(&self, cx: &gpui_kit::App) -> ClockPreference {
        self.owner
            .upgrade()
            .and_then(|owner| {
                owner
                    .read(cx)
                    .send_catalog
                    .settings
                    .as_ref()
                    .map(|settings| settings.clock)
            })
            .unwrap_or_default()
    }

    /// Opening the page reads every provider once per runtime.
    pub(crate) fn open(&mut self, cx: &mut Context<Self>) {
        let Some(runtime) = self.runtime(cx) else {
            return;
        };
        if !self.runtimes.contains_key(&runtime) {
            self.refresh(cx);
        }
    }

    /// The sidebar's refresh: every provider again.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let workspace = owner.read(cx);
        let runtime = workspace.runtime.clone();
        let Some((client, network)) = workspace
            .connection
            .as_ref()
            .and_then(|connection| Some((connection.client.clone()?, connection.network.handle())))
        else {
            return;
        };
        let slot = self.runtimes.entry(runtime.clone()).or_default();
        if slot.pending > 0 {
            return;
        }
        slot.tasks.clear();
        slot.pending = QUOTA_PROVIDERS.len();
        for (provider, _) in QUOTA_PROVIDERS {
            let client = client.clone();
            let read = NetworkTask::spawn_on(&network, async move { client.quota(provider).await });
            let runtime = runtime.clone();
            slot.tasks.push(cx.spawn(async move |this, cx| {
                let answer = match read.await {
                    Some(Ok(result)) => Ok(result),
                    Some(Err(error)) => Err(error.to_string()),
                    None => Err("Failed to fetch quota".to_owned()),
                };
                let _ = this.update(cx, |this, cx| {
                    let Some(slot) = this.runtimes.get_mut(&runtime) else {
                        return;
                    };
                    slot.providers.entry(provider).or_default().apply(answer);
                    slot.pending = slot.pending.saturating_sub(1);
                    if slot.pending == 0 {
                        slot.last_updated = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .ok()
                            .map(|elapsed| elapsed.as_millis() as i64);
                    }
                    if this.selected.is_none() {
                        this.selected = first_configured(slot);
                    }
                    cx.notify();
                });
            }));
        }
        cx.notify();
    }

    fn slot(&self, cx: &gpui_kit::App) -> Option<&RuntimeUsage> {
        self.runtimes.get(&self.runtime(cx)?)
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let slot = self.slot(cx);
        let loading = slot.is_some_and(|slot| slot.pending > 0);
        let mut list = div()
            .id("usage-providers")
            .role(Role::List)
            .aria_label(strings.get("settings.usage.sidebar.title"))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(8.0));
        for (provider, name) in QUOTA_PROVIDERS {
            let state = slot.and_then(|slot| slot.providers.get(provider));
            let result = state.and_then(|state| state.result.as_ref());
            let configured = result.is_some_and(|result| result.configured);
            let dot = if !configured {
                palette.muted_foreground.opacity(0.4)
            } else {
                match usage_tone(result.and_then(QuotaResult::highest_used_percent)) {
                    UsageTone::Critical => palette.status_error,
                    UsageTone::Warn => palette.status_warning,
                    UsageTone::Safe => palette.status_success,
                }
            };
            let selected = self.selected == Some(provider);
            let hover = palette.hover;
            let id: SharedString = format!("usage-provider-{provider}").into();
            list = list.child(
                on_activate(
                    div()
                        .id(id.clone())
                        .accessibility_id(id)
                        .role(Role::ListItem)
                        .aria_label(SharedString::from(name))
                        .aria_selected(selected)
                        .tab_index(0)
                        .px(px(6.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(8.0))
                        .when(selected, |row| row.bg(palette.selection))
                        .when(!selected, |row| row.hover(move |style| style.bg(hover)))
                        .child(
                            div()
                                .size(px(10.0))
                                .rounded(px(5.0))
                                .flex_shrink_0()
                                .bg(dot),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .when(result.is_some_and(|result| !result.configured), |row| {
                            row.child(
                                div()
                                    .text_size(gpui_kit::rems(0.75))
                                    .text_color(palette.muted_foreground)
                                    .child(strings.get("settings.usage.sidebar.status.notSet")),
                            )
                        }),
                    cx,
                    move |this, _, cx| {
                        this.selected = Some(provider);
                        cx.notify();
                    },
                )
                .test_support(),
            );
        }
        let mode_chip = |mode: DisplayMode, key: &'static str, id: &'static str| {
            let selected = self.mode == mode;
            on_activate(
                div()
                    .id(id)
                    .accessibility_id(id)
                    .role(Role::Button)
                    .aria_label(strings.get(key))
                    .aria_toggled(if selected {
                        gpui_kit::Toggled::True
                    } else {
                        gpui_kit::Toggled::False
                    })
                    .px(px(8.0))
                    .h_full()
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .when(selected, |chip| chip.bg(palette.background))
                    .child(strings.get(key)),
                cx,
                move |this, _, cx| {
                    this.mode = mode;
                    cx.notify();
                },
            )
            .test_support()
        };
        div()
            .w(px(260.0))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(palette.elevated)
            .child(
                div()
                    .px(px(12.0))
                    .pt(px(16.0))
                    .pb(px(12.0))
                    .border_b_1()
                    .border_color(palette.border)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .mb(px(12.0))
                            .child(strings.get("settings.usage.sidebar.title")),
                    )
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .justify_between()
                            .child(div().text_color(palette.muted_foreground).child(
                                strings.format(
                                    "settings.usage.sidebar.total",
                                    &[("count", &QUOTA_PROVIDERS.len().to_string())],
                                ),
                            ))
                            .child(
                                IconButton::new(
                                    "usage-refresh",
                                    if loading { "loader-4" } else { "refresh" },
                                    strings.get("settings.usage.sidebar.actions.refreshAria"),
                                )
                                .render(
                                    &palette,
                                    cx,
                                    |this, _, cx| this.refresh(cx),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(8.0))
                            .flex_row_from(direction)
                            .items_center()
                            .justify_between()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .text_size(gpui_kit::rems(0.75))
                                    .text_color(palette.muted_foreground)
                                    .child(strings.get("settings.usage.sidebar.field.display")),
                            )
                            .child(
                                div()
                                    .id("usage-display-mode")
                                    .role(Role::Group)
                                    .h(gpui_kit::rems(1.75))
                                    .p(px(2.0))
                                    .rounded(px(7.0))
                                    .bg(palette.hover)
                                    .flex_row_from(direction)
                                    .gap(px(2.0))
                                    .child(mode_chip(
                                        DisplayMode::Usage,
                                        "settings.usage.sidebar.field.displayModeUsage",
                                        "usage-display-usage",
                                    ))
                                    .child(mode_chip(
                                        DisplayMode::Remaining,
                                        "settings.usage.sidebar.field.displayModeRemaining",
                                        "usage-display-remaining",
                                    )),
                            ),
                    ),
            )
            .child(list)
            .into_any_element()
    }

    fn window_row(
        &self,
        id: String,
        title: String,
        subtitle: Option<String>,
        window: &UsageWindow,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let palette = cx.global::<Palette>();
        let direction = strings.direction();
        let percent = match self.mode {
            DisplayMode::Usage => window.used_percent,
            DisplayMode::Remaining => window.remaining_percent,
        };
        let value = value_label(window, percent, strings);
        let reset = reset_label(window, self.clock(cx), cx);
        let reset = reset.map(|time| {
            strings
                .format("settings.usage.card.resetsAt", &[("time", &time)])
                .to_string()
        });
        let tone = match usage_tone(window.used_percent) {
            UsageTone::Critical => palette.status_error,
            UsageTone::Warn => palette.status_warning,
            UsageTone::Safe => palette.status_success,
        };
        let mut row = div()
            .id(SharedString::from(id.clone()))
            .accessibility_id(id)
            .role(Role::Group)
            .aria_label(SharedString::from(format!("{title} {value}")))
            .py(px(12.0))
            .border_b_1()
            .border_color(palette.border)
            .child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(div().truncate().child(title))
                            .children(subtitle.map(|subtitle| {
                                div()
                                    .truncate()
                                    .text_size(gpui_kit::rems(0.75))
                                    .text_color(palette.muted_foreground)
                                    .child(subtitle)
                            })),
                    )
                    .child(div().child(if value == "-" { String::new() } else { value })),
            );
        let caption = |text: Option<String>| {
            div()
                .text_size(gpui_kit::rems(0.6875))
                .text_color(palette.muted_foreground)
                .child(text.unwrap_or_default())
        };
        if let Some(percent) = percent {
            let fill = (percent / 100.0).clamp(0.0, 1.0) as f32;
            let bar_label = match self.mode {
                DisplayMode::Usage => strings.get("settings.usage.card.bar.used"),
                DisplayMode::Remaining => strings.get("settings.usage.card.bar.remaining"),
            };
            row = row.child(
                div()
                    .mt(px(10.0))
                    .child(
                        div()
                            .h(px(6.0))
                            .w_full()
                            .rounded(px(3.0))
                            .bg(palette.border)
                            .child(
                                div()
                                    .h_full()
                                    .rounded(px(3.0))
                                    .w(gpui_kit::relative(fill))
                                    .bg(tone),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(4.0))
                            .flex_row_from(direction)
                            .justify_between()
                            .child(caption(reset))
                            .child(caption(Some(bar_label.to_string()))),
                    ),
            );
        } else if reset.is_some() {
            row = row.child(div().mt(px(4.0)).child(caption(reset)));
        }
        row.into_any_element()
    }

    fn detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let Some(provider) = self.selected else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(palette.muted_foreground)
                .child(strings.get("settings.usage.page.empty.selectProvider"))
                .into_any_element();
        };
        let name = QUOTA_PROVIDERS
            .iter()
            .find(|(id, _)| *id == provider)
            .map_or(provider, |(_, name)| *name);
        let slot = self.slot(cx);
        let state = slot.and_then(|slot| slot.providers.get(provider));
        let result = state.and_then(|state| state.result.as_ref());
        let loading = slot.is_some_and(|slot| slot.pending > 0);
        let time = slot
            .and_then(|slot| slot.last_updated)
            .and_then(|at| crate::turn_footer::message_time(at, self.clock(cx), cx))
            .map_or_else(|| "-".to_owned(), |time| time.to_string());
        let description = if loading {
            strings.get("settings.usage.page.header.refreshing")
        } else if let Some(plan) = result.and_then(|result| result.plan_label.as_deref()) {
            strings.format(
                "settings.usage.page.header.lastUpdatedWithPlan",
                &[("plan", plan), ("time", &time)],
            )
        } else {
            strings.format("settings.usage.page.header.lastUpdated", &[("time", &time)])
        };
        let mut body = div()
            .id("usage-detail")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(48.0))
            .py(px(32.0))
            .when(direction == Direction::RightToLeft, |view| {
                view.text_right()
            })
            .child(
                div()
                    .id("usage-provider-title")
                    .accessibility_id("usage-provider-title")
                    .role(Role::Heading)
                    .font_weight(FontWeight::SEMIBOLD)
                    .css_text(1.125, 1.5, cx)
                    .child(strings.format(
                        "settings.usage.page.header.providerUsage",
                        &[("provider", name)],
                    )),
            )
            .child(
                div()
                    .id("usage-status")
                    .accessibility_id("usage-status")
                    .role(Role::Status)
                    .aria_label(description.clone())
                    .mt(px(4.0))
                    .mb(px(24.0))
                    .text_color(palette.muted_foreground)
                    .child(description),
            );
        let callout = |id: &'static str, title: SharedString, text: String, color| {
            div()
                .id(id)
                .accessibility_id(id)
                .role(Role::Alert)
                .aria_label(title.clone())
                .mb(px(24.0))
                .px(px(16.0))
                .py(px(12.0))
                .rounded(px(8.0))
                .bg(gpui_kit::Hsla::opacity(&color, 0.12))
                .text_color(color)
                .child(div().font_weight(FontWeight::MEDIUM).child(title))
                .child(div().mt(px(4.0)).child(text))
        };
        let error = state
            .and_then(|state| state.error_line(&strings))
            .or_else(|| {
                result
                    .filter(|result| result.configured)
                    .and_then(|result| result.error.clone())
            });
        if result.is_none() && error.is_none() {
            body = body.child(div().child(strings.get("settings.usage.page.state.noData")));
        }
        if let Some(error) = error {
            body = body.child(callout(
                "usage-error",
                strings.get("settings.usage.page.state.refreshFailedTitle"),
                error,
                palette.status_error,
            ));
        }
        let Some(result) = result else {
            return body.into_any_element();
        };
        if !result.configured {
            body = body.child(callout(
                "usage-not-configured",
                strings.get("settings.usage.page.state.providerNotConfiguredTitle"),
                strings
                    .get("settings.usage.page.state.providerNotConfiguredDescription")
                    .to_string(),
                palette.status_warning,
            ));
        }
        let windows = result.windows.as_ref();
        for (label, window) in windows.into_iter().flatten() {
            body = body.child(self.window_row(
                format!("usage-window-{label}"),
                window_label(label, &strings),
                None,
                window,
                cx,
            ));
        }
        let models: Vec<_> = result
            .models
            .iter()
            .filter_map(|(model, windows)| Some((model, windows.iter().next()?)))
            .collect();
        if !models.is_empty() {
            body = body.child(
                div()
                    .mt(px(32.0))
                    .mb(px(8.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(strings.get("settings.usage.page.section.modelQuotas")),
            );
            for (model, (label, window)) in models {
                body = body.child(self.window_row(
                    format!("usage-model-{model}"),
                    window_label(label, &strings),
                    Some(model.clone()),
                    window,
                    cx,
                ));
            }
        }
        if result.configured
            && windows.is_some_and(|windows| windows.is_empty())
            && result.models.is_empty()
        {
            body = body.child(
                div()
                    .child(strings.get("settings.usage.page.state.noQuotaWindowsTitle"))
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_color(palette.muted_foreground)
                            .child(
                                strings.get("settings.usage.page.state.noQuotaWindowsDescription"),
                            ),
                    ),
            );
        }
        body.into_any_element()
    }
}

fn first_configured(slot: &RuntimeUsage) -> Option<&'static str> {
    QUOTA_PROVIDERS
        .iter()
        .find(|(provider, _)| {
            slot.providers
                .get(provider)
                .and_then(|state| state.result.as_ref())
                .is_some_and(|result| result.configured)
        })
        .map(|(provider, _)| *provider)
}

/// `formatWindowLabel`.
pub(crate) fn window_label(label: &str, strings: &Strings) -> String {
    let key = match label {
        "5h" => "quota.window.5h",
        "7d" => "quota.window.7d",
        "extra_usage" => "quota.window.extraUsage",
        "weekly" => "quota.window.weekly",
        "daily" => "quota.window.daily",
        "monthly" => "quota.window.monthly",
        "credits" => "quota.window.credits",
        "key_limit" => "quota.window.keyLimit",
        "credits_balance" => "quota.window.creditsBalance",
        "monthly_credits" => "quota.window.monthlyCredits",
        "purchased_credits" => "quota.window.purchasedCredits",
        "free_credits" => "quota.window.freeCredits",
        "billing_cycle" => "quota.window.billingCycle",
        "plan_limit" => "quota.window.planLimit",
        "auto" => "quota.window.auto",
        "api" => "quota.window.api",
        "on_demand" => "quota.window.onDemand",
        "session" => "quota.window.session",
        "premium" => "quota.window.premium",
        "chat" => "quota.window.chat",
        "completions" => "quota.window.completions",
        "premium_interactions" => "quota.window.premiumInteractions",
        _ => return label.to_owned(),
    };
    strings.get(key).to_string()
}

/// `formatQuotaWindowValueLabel`. Money is written in US dollars with two
/// decimals; the web's `Intl` grouping is not reproduced.
pub(crate) fn value_label(window: &UsageWindow, percent: Option<f64>, strings: &Strings) -> String {
    match &window.value {
        Some(WindowValue::Requests(count)) => strings
            .format("quota.value.requests", &[("count", &format!("{count}"))])
            .to_string(),
        Some(WindowValue::Spend { spent, limit }) => {
            let spent = format!("${spent:.2}");
            match limit {
                None => strings.format("quota.value.spent", &[("spent", &spent)]),
                Some(limit) => strings.format(
                    "settings.usage.consoleBudgets.spentOf",
                    &[("spent", &spent), ("limit", &format!("${limit:.2}"))],
                ),
            }
            .to_string()
        }
        None => match (window.credits_used, &window.value_label) {
            (Some(credits), _) => {
                let rounded = (credits * 10.0).round() / 10.0;
                strings
                    .format(
                        "quota.value.creditsUsed",
                        &[("credits", &format!("{rounded}"))],
                    )
                    .to_string()
            }
            (None, Some(label)) => label.clone(),
            (None, None) => percent.map_or_else(|| "-".to_owned(), |p| format!("{}%", p.round())),
        },
    }
}

/// `formatQuotaResetLabel`: the reset's time, with its day when not today,
/// else the server's own text.
fn reset_label(
    window: &UsageWindow,
    clock: ClockPreference,
    cx: &Context<UsagePage>,
) -> Option<String> {
    match window.reset_at {
        Some(at) if at.is_finite() => {
            crate::turn_footer::message_time(at as i64, clock, cx).map(|text| text.to_string())
        }
        _ => window
            .reset_after_formatted
            .clone()
            .or_else(|| window.reset_at_formatted.clone()),
    }
    .filter(|text| !text.is_empty())
}

impl Render for UsagePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let sidebar = self.sidebar(cx);
        let detail = self.detail(cx);
        div()
            .id("usage-settings-page")
            .accessibility_id("usage-settings-page")
            .role(Role::Group)
            .aria_label(strings.get("settings.page.usage.title"))
            .size_full()
            .flex_row_from(strings.direction())
            .css_text(0.875, 1.25, cx)
            .text_color(palette.foreground)
            .child(sidebar)
            .child(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(configured: bool, ok: bool, used: Option<f64>) -> QuotaResult {
        QuotaResult {
            provider_id: "codex".into(),
            provider_name: "Codex".into(),
            ok,
            configured,
            error: (!ok).then(|| "429".into()),
            windows: used.map(|used| {
                [(
                    "5h".to_owned(),
                    UsageWindow {
                        used_percent: Some(used),
                        ..UsageWindow::default()
                    },
                )]
                .into()
            }),
            ..QuotaResult::default()
        }
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_usage_and_says_it_is_stale() {
        let strings = Strings::load("en");
        let mut state = ProviderState::default();
        state.apply(Ok(result(true, true, Some(40.0))));
        assert_eq!(state.error_line(&strings), None);
        state.apply(Ok(result(true, false, None)));
        assert_eq!(
            state
                .result
                .as_ref()
                .and_then(QuotaResult::highest_used_percent),
            Some(40.0),
            "the provider's failure does not erase its last sample"
        );
        assert_eq!(
            state.error_line(&strings).as_deref(),
            Some("Showing previously received usage. Refresh failed: 429")
        );
        state.apply(Err("connection refused".into()));
        assert!(state.result.is_some(), "a transport failure keeps it too");
        state.apply(Ok(result(true, true, Some(10.0))));
        assert_eq!(state.error_line(&strings), None);
        let mut fresh = ProviderState::default();
        fresh.apply(Err("connection refused".into()));
        assert_eq!(
            fresh.error_line(&strings).as_deref(),
            Some("connection refused"),
            "without usage the failure is not called stale"
        );
    }

    #[test]
    fn labels_follow_the_web_formatters() {
        let strings = Strings::load("en");
        assert_eq!(window_label("5h", &strings), "5-Hour");
        assert_eq!(window_label("custom", &strings), "custom");
        let window = UsageWindow::default();
        assert_eq!(value_label(&window, Some(42.4), &strings), "42%");
        assert_eq!(value_label(&window, None, &strings), "-");
        let requests = UsageWindow {
            value: Some(WindowValue::Requests(12.0)),
            ..UsageWindow::default()
        };
        assert_eq!(value_label(&requests, None, &strings), "Requests: 12");
        let credits = UsageWindow {
            credits_used: Some(3.26),
            ..UsageWindow::default()
        };
        assert!(value_label(&credits, None, &strings).contains("3.3"));
    }
}
