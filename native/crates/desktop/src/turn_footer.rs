//! The facts under a finished turn: the provider logo and model, the effort,
//! the agent and the turn's duration, as `MessageBody.tsx` lays them out.
//!
//! The row reserves the width of the message actions, which the web shows on
//! hover and lays out at all times, so the facts fit as they do there. The
//! time fact is the step's completion (else its creation) in the local time
//! zone, written with the locale's exported `Intl` data.

use std::sync::Arc;

use chrono::{Datelike, Local, NaiveDate, TimeZone, Timelike};
use gpui_kit::{
    AnyElement, App, Context, FontFeatures, Hsla, InteractiveElement, IntoElement, ParentElement,
    Pixels, Role, SharedString, StatefulInteractiveElement, Styled, TextAlign, Window, canvas, div,
    point, prelude::FluentBuilder, px,
};
use openchamber_native_core::message_time::{ClockPreference, LocalDate, LocalDateTime};
use openchamber_native_core::send_config::{self, ModelChoice};
use openchamber_native_core::transcript::{Message, PartContent};
use openchamber_native_core::turn_footer::{self, FactWidths, ShownFacts};

use crate::baseline::CssTextExt;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// `mt-2 mb-1`, the row's 26 px action buttons and its `gap-2`.
const MARGIN_TOP: f32 = 8.0;
const MARGIN_BOTTOM: f32 = 4.0;
const ROW_HEIGHT: f32 = 26.0;
const ROW_GAP: f32 = 8.0;
/// The gap to the changed-files line, which a turn that stopped lays out
/// even when it lists nothing.
const FILES_GAP: f32 = 6.0;
/// Copy, continue, pin and more: four 26 px buttons with `gap-1.5`.
const ACTIONS_WIDTH: f32 = 4.0 * 26.0 + 3.0 * 6.0;
/// `text-sm`, in rems of the 16 px root, and its pixels for measuring.
const TEXT_SIZE: f32 = 0.875;
const TEXT_LINE: f32 = 1.25;
const TEXT_PX: f32 = 14.0;
/// The logo or agent icon (`h-3.5 w-3.5`) and the facts' `gap-1.5` and
/// `padding-inline-start: 0.375rem`.
const ICON: f32 = 14.0;
const FACT_GAP: f32 = 6.0;
/// `text-muted-foreground/60`, and the separators' `opacity-60` on top.
const TEXT_ALPHA: f32 = 0.6;
const SEPARATOR_OPACITY: f32 = 0.6;
const SEPARATOR: &str = "·";

/// The facts a finished turn shows.
struct Facts {
    model: ModelChoice,
    model_name: SharedString,
    effort: Option<SharedString>,
    agent: Option<SharedString>,
    duration: Option<SharedString>,
    time: Option<SharedString>,
    /// Whether the turn stopped, which lays out the changed-files line.
    stopped: bool,
}

impl Workspace {
    /// The footer under `message` at `row` of the selected session, when it
    /// ends a turn with text and a stop or an error.
    pub(crate) fn render_turn_footer(
        &self,
        row: usize,
        message: &Message,
        palette: &Palette,
        direction: Direction,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let facts = self.turn_facts(row, message, cx)?;
        let measure = |text: &str| text_width(text, window, false);
        let measure_digits = |text: &str| text_width(text, window, true);
        let fact_width = |text: &str| FACT_GAP + measure(SEPARATOR) + FACT_GAP + measure(text);
        let widths = FactWidths {
            model: ICON + FACT_GAP + measure(&facts.model_name),
            effort: facts.effort.as_deref().map(fact_width),
            agent: facts.agent.as_deref().map(fact_width),
            duration: facts
                .duration
                .as_deref()
                .map(|text| FACT_GAP + measure(SEPARATOR) + FACT_GAP + measure_digits(text)),
            timestamp: facts
                .time
                .as_deref()
                .map(|text| FACT_GAP + measure(SEPARATOR) + FACT_GAP + measure_digits(text)),
        };
        let available = self.composer_width(window) - ROW_GAP - ACTIONS_WIDTH;
        let ShownFacts {
            effort,
            agent,
            timestamp,
        } = turn_footer::fit_facts(available, widths);
        let color = palette.muted_foreground.opacity(TEXT_ALPHA);
        let fact =
            |text: SharedString| render_fact(text, color, Digits::Proportional, direction, cx);
        let digits = |text: SharedString| render_fact(text, color, Digits::Tabular, direction, cx);
        let icon = self
            .render_provider_logo(&facts.model.provider_id, ICON, 1.0, palette)
            .unwrap_or_else(|| div().size(px(ICON)).flex_shrink_0().into_any_element());
        let inline = render_inline_facts(
            facts.model_name.clone(),
            [
                facts
                    .effort
                    .clone()
                    .filter(|_| effort)
                    .map(|text| (text, Digits::Proportional)),
                facts
                    .agent
                    .clone()
                    .filter(|_| agent)
                    .map(|text| (text, Digits::Proportional)),
                facts.duration.clone().map(|text| (text, Digits::Tabular)),
                facts
                    .time
                    .clone()
                    .filter(|_| timestamp)
                    .map(|text| (text, Digits::Tabular)),
            ]
            .into_iter()
            .flatten()
            .collect(),
            color,
            available - ICON - FACT_GAP,
            direction,
            window,
            cx,
        );
        let content = if let Some(inline) = inline {
            div()
                .flex_1()
                .min_w_0()
                .flex_row_from(direction)
                .items_center()
                .gap(px(FACT_GAP))
                .child(icon)
                .child(inline)
        } else {
            let model = div()
                .min_w_0()
                .flex_shrink(1.0)
                .flex_row_from(direction)
                .items_center()
                .gap(px(FACT_GAP))
                .child(icon)
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .css_text(TEXT_SIZE, TEXT_LINE, cx)
                        .child(facts.model_name),
                );
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex_row_from(direction)
                .items_center()
                .whitespace_nowrap()
                .text_color(color)
                .child(model)
                .children(facts.effort.filter(|_| effort).map(&fact))
                .children(facts.agent.filter(|_| agent).map(&fact))
                .children(facts.duration.map(&digits))
                .children(facts.time.filter(|_| timestamp).map(&digits))
        };
        let row = div()
            .h(px(ROW_HEIGHT))
            .flex_row_from(direction)
            .items_center()
            .justify_between()
            .gap(px(ROW_GAP))
            .child(content)
            // The message actions appear on hover; their place is kept.
            .child(div().w(px(ACTIONS_WIDTH)).flex_shrink_0());
        Some(
            div()
                .mt(px(MARGIN_TOP))
                .mb(px(MARGIN_BOTTOM))
                .child(row)
                .when(facts.stopped, |this| this.pb(px(FILES_GAP)))
                .into_any_element(),
        )
    }

    fn turn_facts(&self, row: usize, message: &Message, cx: &Context<Self>) -> Option<Facts> {
        let open = self.open_sessions.get(self.selected.as_ref()?)?;
        let position = open.transcript.turn_position(row)?;
        let assistant = message.info.assistant.as_ref()?;
        let stopped = assistant.outcome.finish.as_deref() == Some("stop");
        let error = assistant
            .outcome
            .error
            .as_deref()
            .is_some_and(|error| !error.trim().is_empty());
        let has_text = error
            || message.parts.iter().any(|part| {
                matches!(&part.content, PartContent::Text { text } if !text.trim().is_empty())
            });
        if !position.last_assistant || !has_text || !(stopped || error) {
            return None;
        }
        let session = self.selected_session();
        let model = assistant
            .model
            .clone()
            .or_else(|| session.and_then(|session| session.model.clone()))?;
        let providers = session
            .and_then(|session| session.directory.as_deref())
            .and_then(openchamber_native_core::path::normalize)
            .and_then(|directory| self.send_catalog.directory(&directory))
            .and_then(|entry| entry.providers.ready())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let time = message_time(
            message.info.completed.unwrap_or(message.info.created),
            self.send_catalog
                .settings
                .as_ref()
                .map(|settings| settings.clock)
                .unwrap_or_default(),
            cx,
        );
        let duration = position
            .prompt_created
            .zip(message.info.completed)
            .filter(|(prompt, completed)| stopped && completed > prompt)
            .map(|(prompt, completed)| turn_footer::turn_duration(completed - prompt).into());
        Some(Facts {
            model_name: send_config::model_label(providers, &model).into(),
            effort: model
                .variant
                .as_deref()
                .and_then(turn_footer::effort_fact)
                .map(SharedString::from),
            agent: assistant
                .agent
                .clone()
                .or_else(|| session.and_then(|session| session.agent.clone()))
                .map(SharedString::from),
            duration,
            time,
            model,
            stopped,
        })
    }
}

/// How a fact's digits are set: `tabular-nums` for the duration and time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Digits {
    Proportional,
    Tabular,
}

fn tabular() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".to_owned(), 1)]))
}

/// The message time as `formatTimestampForDisplay` writes it, in the local
/// time zone; `None` for a time the zone cannot place.
fn message_time(
    timestamp_ms: i64,
    clock: ClockPreference,
    cx: &Context<Workspace>,
) -> Option<SharedString> {
    let moment = Local.timestamp_millis_opt(timestamp_ms).single()?;
    let today = Local::now().date_naive();
    let local_date = |date: NaiveDate| LocalDate {
        year: date.year(),
        month: date.month(),
        day: date.day(),
    };
    let strings = locale::strings(cx);
    let text = strings.dates().message_time(
        &LocalDateTime {
            date: local_date(moment.date_naive()),
            hour: moment.hour(),
            minute: moment.minute(),
        },
        local_date(today),
        local_date(today.pred_opt()?),
        clock,
        |time| {
            strings
                .format("common.date.yesterdayWithTime", &[("time", time)])
                .to_string()
        },
    );
    Some(text.into())
}

/// Shapes the fitted footer as one row. Text widths retain fractional pixels
/// between fragments, avoiding accumulated rounding from separate layout
/// nodes. The existing elements handle truncation when the model must shrink.
fn render_inline_facts(
    model: SharedString,
    facts: Vec<(SharedString, Digits)>,
    color: Hsla,
    available: f32,
    direction: Direction,
    window: &Window,
    cx: &App,
) -> Option<AnyElement> {
    let shape = |text: SharedString, digits: Digits, color: Hsla| {
        let mut style = window.text_style();
        style.color = color;
        if digits == Digits::Tabular {
            style.font_features = tabular();
        }
        let run = style.to_run(text.len());
        window
            .text_system()
            .shape_line(text, px(TEXT_PX), &[run], None)
    };
    let mut label = model.to_string();
    let mut fragments = vec![(shape(model, Digits::Proportional, color), 0.0)];
    for (text, digits) in facts {
        label.push_str(" · ");
        label.push_str(&text);
        fragments.push((
            shape(
                SEPARATOR.into(),
                Digits::Proportional,
                color.opacity(SEPARATOR_OPACITY),
            ),
            FACT_GAP,
        ));
        fragments.push((shape(text, digits, color), FACT_GAP));
    }
    let width: f32 = fragments
        .iter()
        .map(|(line, gap)| f32::from(line.width) + gap)
        .sum();
    if width > available || available <= 0.0 {
        return None;
    }
    let text = canvas(
        |_, _, _| (),
        move |bounds, (), window, cx| {
            let mut x = if direction == Direction::RightToLeft {
                bounds.right()
            } else {
                bounds.left()
            };
            for (line, gap) in &fragments {
                let origin_x = fragment_start(&mut x, line.width, px(*gap), direction);
                if let Err(error) = line.paint(
                    point(origin_x, bounds.top()),
                    px(TEXT_LINE * 16.0),
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                ) {
                    tracing::debug!(%error, "footer text could not be painted");
                }
            }
        },
    )
    .size_full()
    .css_text(TEXT_SIZE, TEXT_LINE, cx);
    Some(
        div()
            .id("footer-facts")
            .flex_1()
            .min_w_0()
            .h(px(TEXT_LINE * 16.0))
            .role(Role::Label)
            .aria_label(label)
            .child(text)
            .into_any_element(),
    )
}

fn fragment_start(cursor: &mut Pixels, width: Pixels, gap: Pixels, direction: Direction) -> Pixels {
    match direction {
        Direction::LeftToRight => {
            *cursor += gap;
            let start = *cursor;
            *cursor += width;
            start
        }
        Direction::RightToLeft => {
            *cursor -= gap + width;
            *cursor
        }
    }
}

/// One fact after the model: its separator and text.
fn render_fact(
    text: SharedString,
    color: Hsla,
    digits: Digits,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    div()
        .flex_none()
        .flex_row_from(direction)
        .items_center()
        .gap(px(FACT_GAP))
        .padding_start(direction, px(FACT_GAP))
        .child(
            div()
                .css_text(TEXT_SIZE, TEXT_LINE, cx)
                .text_color(color.opacity(SEPARATOR_OPACITY))
                .child(SEPARATOR),
        )
        .child(
            div()
                .css_text(TEXT_SIZE, TEXT_LINE, cx)
                .when(digits == Digits::Tabular, |this| {
                    this.font_features(tabular())
                })
                .child(text),
        )
        .into_any_element()
}

/// The width of `text` in the UI font at the footer's size, with tabular
/// digits when `tabular` is set.
fn text_width(text: &str, window: &Window, tabular_digits: bool) -> f32 {
    let mut style = window.text_style();
    if tabular_digits {
        style.font_features = tabular();
    }
    let run = style.to_run(text.len());
    f32::from(
        window
            .text_system()
            .layout_line(text, px(TEXT_PX), &[run], None)
            .width,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_facts_keep_fractional_widths_and_mirror_positions() {
        let mut left = px(10.0);
        let mut right = px(100.0);
        for (width, gap) in [(1.25, 0.0), (2.4, 6.0), (0.5, 6.0)] {
            let ltr = fragment_start(&mut left, px(width), px(gap), Direction::LeftToRight);
            let rtl = fragment_start(&mut right, px(width), px(gap), Direction::RightToLeft);
            assert!(
                (f32::from(ltr + rtl) + width - 110.0).abs() < 0.0001,
                "each fragment mirrors across the same bounds"
            );
        }
        assert!(
            (f32::from(left) - 26.15).abs() < 0.0001,
            "layout never rounds an intermediate width"
        );
    }
}
