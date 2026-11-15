//! The facts under a finished turn: the provider logo and model, the agent
//! and the turn's duration, as `MessageBody.tsx` lays them out.
//!
//! The facts are as wide as their text and the message actions follow them,
//! shown on hover and laid out at all times, so the facts fit as they do on
//! the web. The thinking effort is the model's tooltip and the time the
//! duration's: the step's completion (else its creation) in the local time
//! zone, written with the locale's exported `Intl` data.

use std::sync::Arc;

use chrono::{Datelike, Local, NaiveDate, TimeZone, Timelike};
use gpui_kit::component::tooltip::Tooltip;
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
use crate::message_actions::ACTIONS_WIDTH;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// `mt-2 mb-1`, the row's 26 px action buttons and its `gap-3`.
const MARGIN_TOP: f32 = 8.0;
const MARGIN_BOTTOM: f32 = 4.0;
const ROW_HEIGHT: f32 = 26.0;
const ROW_GAP: f32 = 12.0;
/// The gap to the changed-files line, which a turn that stopped lays out
/// even when it lists nothing.
const FILES_GAP: f32 = 6.0;
/// `typography-meta`: `--text-meta` on a `round(1.45em, 1px)` line, in rems
/// of the 16 px root, and the size in pixels for measuring.
const TEXT_SIZE: f32 = 0.78125;
const TEXT_LINE: f32 = 1.125;
const TEXT_PX: f32 = 12.5;
/// The logo (`h-[13px] w-[13px]`) and the facts' `gap-1.5` and
/// `padding-inline-start: 0.375rem`.
const ICON: f32 = 13.0;
const FACT_GAP: f32 = 6.0;
/// The facts are `text-muted-foreground`; the separators add `opacity-60`.
const SEPARATOR_OPACITY: f32 = 0.6;
const SEPARATOR: &str = "·";

/// The facts a finished turn shows. The effort and the time are tooltips.
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
        let widths = FactWidths {
            model: ICON + FACT_GAP + measure(&facts.model_name),
            agent: facts
                .agent
                .as_deref()
                .map(|text| FACT_GAP + measure(SEPARATOR) + FACT_GAP + measure(text)),
            duration: facts
                .duration
                .as_deref()
                .map(|text| FACT_GAP + measure(SEPARATOR) + FACT_GAP + measure_digits(text)),
        };
        let available = self.composer_width(window) - ROW_GAP - ACTIONS_WIDTH;
        let ShownFacts { agent } = turn_footer::fit_facts(available, widths);
        let color = palette.muted_foreground;
        let effort_hint = facts.effort.as_deref().map(|effort| {
            locale::strings(cx).format(
                "chat.messageBody.footer.reasoningTooltip",
                &[("effort", effort)],
            )
        });
        let time_hint = facts.time.clone();
        let icon = self
            .render_provider_logo(&facts.model.provider_id, ICON, 1.0, palette)
            .unwrap_or_else(|| div().size(px(ICON)).flex_shrink_0().into_any_element());
        let inline = render_inline_facts(
            InlineFacts {
                model: facts.model_name.clone(),
                agent: facts.agent.clone().filter(|_| agent),
                duration: facts.duration.clone(),
                model_hint: effort_hint.clone(),
                duration_hint: time_hint.clone(),
            },
            color,
            available - ICON - FACT_GAP,
            direction,
            window,
            cx,
        );
        // `flex: 0 1 auto`: as wide as the facts, so the actions follow them.
        let content = if let Some(inline) = inline {
            div()
                .min_w_0()
                .flex_shrink(1.0)
                .flex_row_from(direction)
                .items_center()
                .gap(px(FACT_GAP))
                .child(icon)
                .child(inline)
        } else {
            let model = div()
                .id("footer-model")
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
                )
                .when_some(effort_hint, |this, hint| {
                    this.tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
                });
            div()
                .min_w_0()
                .flex_shrink(1.0)
                .overflow_hidden()
                .flex_row_from(direction)
                .items_center()
                .whitespace_nowrap()
                .text_color(color)
                .child(model)
                .children(facts.agent.filter(|_| agent).map(|text| {
                    render_fact(text, color, Digits::Proportional, None, direction, cx)
                }))
                .children(facts.duration.map(|text| {
                    render_fact(text, color, Digits::Tabular, time_hint, direction, cx)
                }))
        };
        let row = div()
            .h(px(ROW_HEIGHT))
            .flex_row_from(direction)
            .items_center()
            .gap(px(ROW_GAP))
            .child(content)
            // The message actions appear on hover; their place is kept.
            .child(self.render_message_actions(row, message, palette, direction, cx));
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

/// How a fact's digits are set: `tabular-nums` for the duration.
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
pub(crate) fn message_time<V>(
    timestamp_ms: i64,
    clock: ClockPreference,
    cx: &Context<V>,
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

/// The facts drawn on one line and the tooltips two of them carry.
struct InlineFacts {
    model: SharedString,
    agent: Option<SharedString>,
    duration: Option<SharedString>,
    model_hint: Option<SharedString>,
    duration_hint: Option<SharedString>,
}

/// Shapes the fitted footer as one row. Text widths retain fractional pixels
/// between fragments, avoiding accumulated rounding from separate layout
/// nodes. The existing elements handle truncation when the model must shrink.
/// The model and the duration carry their tooltips on areas over the text.
fn render_inline_facts(
    facts: InlineFacts,
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
    let mut label = facts.model.to_string();
    let has_duration = facts.duration.is_some();
    let mut fragments = vec![(shape(facts.model, Digits::Proportional, color), 0.0)];
    let later = [
        facts.agent.map(|text| (text, Digits::Proportional)),
        facts.duration.map(|text| (text, Digits::Tabular)),
    ];
    for (text, digits) in later.into_iter().flatten() {
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
    let model_width = f32::from(fragments[0].0.width);
    let duration_width = fragments
        .last()
        .filter(|_| has_duration)
        .map(|(line, _)| f32::from(line.width));
    // A tooltip's area: `span` wide, `start` from the row's inline start.
    let area = |id: &'static str, start: f32, span: f32, hint: SharedString| {
        let left = match direction {
            Direction::LeftToRight => start,
            Direction::RightToLeft => width - start - span,
        };
        div()
            .id(id)
            .absolute()
            .top_0()
            .left(px(left))
            .w(px(span))
            .h_full()
            .tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
    };
    let model_area = facts
        .model_hint
        .map(|hint| area("footer-model", 0.0, model_width, hint));
    let duration_area = duration_width
        .zip(facts.duration_hint)
        .map(|(span, hint)| area("footer-duration", width - span, span, hint));
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
            .relative()
            .flex_none()
            .w(px(width))
            .h(px(TEXT_LINE * 16.0))
            .role(Role::Label)
            .aria_label(label)
            .child(text)
            .children(model_area)
            .children(duration_area)
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

/// One fact after the model: its separator and text, with `hint` as its
/// tooltip.
fn render_fact(
    text: SharedString,
    color: Hsla,
    digits: Digits,
    hint: Option<SharedString>,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    div()
        .id(SharedString::from(format!("footer-fact:{text}")))
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
        .when_some(hint, |this, hint| {
            this.tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
        })
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

    /// `typography-meta` at the 16 px root: 12.5 px text on an 18 px line,
    /// `round(1.45em, 1px)`.
    #[test]
    fn facts_use_the_meta_text_size_and_line() {
        assert_eq!(TEXT_SIZE * 16.0, TEXT_PX);
        assert_eq!(TEXT_LINE * 16.0, (1.45 * TEXT_PX).round());
        assert_eq!(TEXT_LINE * 16.0, 18.0);
    }
}
