//! Settings' Usage page (`UsagePage.tsx`, `UsageSidebar.tsx`): every quota
//! provider in a list with its fullest window's tone, and the selected
//! provider's windows and per-model windows.
//!
//! The quotas live in `usage_quota.rs`, shared with the work status card: a
//! refresh that fails keeps the last answer on screen and says so, and one
//! failing provider leaves the others' answers. Provider logos, credential
//! forms, Claude Code sign-in, gift resets, console budgets, the work status
//! toggle and model family grouping are not ported to this page.

use gpui_kit::{
    AnyElement, App, Context, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Render, Role, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Subscription,
    TestSupportExt, WeakEntity, Window, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::message_time::ClockPreference;
use openchamber_native_transport::quota::{
    QUOTA_PROVIDERS, UsageTone, UsageWindow, WindowValue, usage_tone,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::IconButton;
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::usage_quota::{DisplayMode, ProviderState, UsageQuota};
use crate::workspace::Workspace;

pub(crate) struct UsagePage {
    owner: WeakEntity<Workspace>,
    /// The workspace's quotas, observed from the page's first opening.
    quota: Option<(Entity<UsageQuota>, Subscription)>,
    selected: Option<&'static str>,
    scroll: ScrollHandle,
}

impl UsagePage {
    pub(crate) fn new(owner: WeakEntity<Workspace>) -> Self {
        Self {
            owner,
            quota: None,
            selected: None,
            scroll: ScrollHandle::new(),
        }
    }

    fn clock(&self, cx: &App) -> ClockPreference {
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

    /// Opening the page reads every provider once per runtime. The workspace
    /// opens Settings from its own update, so its quotas are reached after.
    pub(crate) fn open(&mut self, cx: &mut Context<Self>) {
        let page = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = page.update(cx, |page, cx| {
                if let Some(quota) = page.attach(cx) {
                    quota.update(cx, |quota, cx| quota.ensure_all(cx));
                }
            });
        });
    }

    fn attach(&mut self, cx: &mut Context<Self>) -> Option<Entity<UsageQuota>> {
        if self.quota.is_none() {
            let quota = self.owner.upgrade()?.read(cx).usage_quota.clone();
            let subscription = cx.observe(&quota, |page, quota, cx| {
                let quota = quota.read(cx);
                // Another runtime's providers are not this selection's.
                if page.selected.is_none() || quota.provider_count() == 0 {
                    page.selected = first_configured(quota);
                }
                cx.notify();
            });
            self.quota = Some((quota, subscription));
        }
        self.quota.as_ref().map(|(quota, _)| quota.clone())
    }

    /// The sidebar's refresh: every provider again.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(quota) = self.attach(cx) {
            quota.update(cx, |quota, cx| quota.refresh_all(cx));
        }
    }

    fn quota<'a>(&self, cx: &'a App) -> Option<&'a UsageQuota> {
        self.quota.as_ref().map(|(quota, _)| quota.read(cx))
    }

    /// The mode chosen here in this run, else the saved `usageDisplayMode`.
    fn mode(&self, cx: &App) -> DisplayMode {
        let saved = self.owner.upgrade().and_then(|owner| {
            owner
                .read(cx)
                .send_catalog
                .settings
                .as_ref()
                .and_then(|settings| settings.usage_remaining)
        });
        let saved = match saved {
            Some(true) => DisplayMode::Remaining,
            _ => DisplayMode::Usage,
        };
        self.quota(cx)
            .map_or(saved, |quota| quota.display_mode(saved))
    }

    fn set_mode(&mut self, mode: DisplayMode, cx: &mut Context<Self>) {
        if let Some(quota) = self.attach(cx) {
            quota.update(cx, |quota, cx| quota.set_display_mode(mode, cx));
        }
        cx.notify();
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let loading = self.quota(cx).is_some_and(UsageQuota::is_loading);
        let mode = self.mode(cx);
        // Whether each provider is configured, with its fullest window.
        let answers: Vec<Option<(bool, Option<f64>)>> = QUOTA_PROVIDERS
            .iter()
            .map(|(provider, _)| {
                let result = self.quota(cx)?.provider(provider)?.result.as_ref()?;
                Some((result.configured, result.highest_used_percent()))
            })
            .collect();
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
        for ((provider, name), answer) in QUOTA_PROVIDERS.into_iter().zip(answers) {
            let configured = answer.is_some_and(|(configured, _)| configured);
            let dot = if !configured {
                palette.muted_foreground.opacity(0.4)
            } else {
                match usage_tone(answer.and_then(|(_, highest)| highest)) {
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
                        .when(answer.is_some_and(|(configured, _)| !configured), |row| {
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
        let mode_chip = |chip: DisplayMode, key: &'static str, id: &'static str| {
            let selected = mode == chip;
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
                move |this, _, cx| this.set_mode(chip, cx),
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
        mode: DisplayMode,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let palette = cx.global::<Palette>();
        let direction = strings.direction();
        let percent = mode.percent(window);
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
            let bar_label = match mode {
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
        let mode = self.mode(cx);
        let state: Option<ProviderState> = self
            .quota(cx)
            .and_then(|quota| quota.provider(provider).cloned());
        let state = state.as_ref();
        let result = state.and_then(|state| state.result.as_ref());
        let loading = self.quota(cx).is_some_and(UsageQuota::is_loading);
        let time = self
            .quota(cx)
            .and_then(UsageQuota::last_updated)
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
            .child({
                let title = strings.format(
                    "settings.usage.page.header.providerUsage",
                    &[("provider", name)],
                );
                div()
                    .id("usage-provider-title")
                    .accessibility_id("usage-provider-title")
                    .role(Role::Heading)
                    .aria_label(title.clone())
                    .font_weight(FontWeight::SEMIBOLD)
                    .css_text(1.125, 1.5, cx)
                    .child(title)
            })
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
                mode,
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
                    mode,
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

fn first_configured(quota: &UsageQuota) -> Option<&'static str> {
    QUOTA_PROVIDERS
        .iter()
        .find(|(provider, _)| {
            quota
                .provider(provider)
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
pub(crate) fn reset_label<V>(
    window: &UsageWindow,
    clock: ClockPreference,
    cx: &Context<V>,
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
    use openchamber_native_transport::quota::QuotaResult;

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
