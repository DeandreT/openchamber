//! The facts under a finished turn: the provider logo and model, the effort,
//! the agent and the turn's duration, as `MessageBody.tsx` lays them out.
//!
//! The row reserves the width of the message actions, which the web shows on
//! hover and lays out at all times, so the facts fit as they do there. The
//! time fact is left out: formatting local time needs a time-zone source the
//! native workspace does not have yet.

use gpui_kit::{
    AnyElement, Context, Hsla, IntoElement, ParentElement, SharedString, Styled, Window, div, px,
};
use openchamber_native_core::send_config::{self, ModelChoice};
use openchamber_native_core::transcript::{Message, PartContent};
use openchamber_native_core::turn_footer::{self, FactWidths, ShownFacts};

use crate::baseline::CssTextExt;
use crate::locale::Direction;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// `mt-2 mb-1`, the row's 26 px action buttons and its `gap-2`.
const MARGIN_TOP: f32 = 8.0;
const MARGIN_BOTTOM: f32 = 4.0;
const ROW_HEIGHT: f32 = 26.0;
const ROW_GAP: f32 = 8.0;
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
        let facts = self.turn_facts(row, message)?;
        let measure = |text: &str| text_width(text, window);
        let fact_width = |text: &str| FACT_GAP + measure(SEPARATOR) + FACT_GAP + measure(text);
        let widths = FactWidths {
            model: ICON + FACT_GAP + measure(&facts.model_name),
            effort: facts.effort.as_deref().map(fact_width),
            agent: facts.agent.as_deref().map(fact_width),
            duration: facts.duration.as_deref().map(fact_width),
            timestamp: None,
        };
        let available = self.composer_width(window) - ROW_GAP - ACTIONS_WIDTH;
        let ShownFacts { effort, agent, .. } = turn_footer::fit_facts(available, widths);
        let color = palette.muted_foreground.opacity(TEXT_ALPHA);
        let fact = |text: SharedString| render_fact(text, color, direction, cx);
        let icon = self
            .render_provider_logo(&facts.model.provider_id, ICON, 1.0, palette)
            .unwrap_or_else(|| div().size(px(ICON)).flex_shrink_0().into_any_element());
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
        let row = div()
            .h(px(ROW_HEIGHT))
            .flex_row_from(direction)
            .items_center()
            .justify_between()
            .gap(px(ROW_GAP))
            .child(
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
                    .children(facts.duration.map(&fact)),
            )
            // The message actions appear on hover; their place is kept.
            .child(div().w(px(ACTIONS_WIDTH)).flex_shrink_0());
        Some(
            div()
                .mt(px(MARGIN_TOP))
                .mb(px(MARGIN_BOTTOM))
                .child(row)
                .into_any_element(),
        )
    }

    fn turn_facts(&self, row: usize, message: &Message) -> Option<Facts> {
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
            model,
        })
    }
}

/// One fact after the model: its separator and text.
fn render_fact(
    text: SharedString,
    color: Hsla,
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
        .child(div().css_text(TEXT_SIZE, TEXT_LINE, cx).child(text))
        .into_any_element()
}

/// The width of `text` in the UI font at the footer's size.
fn text_width(text: &str, window: &Window) -> f32 {
    let run = window.text_style().to_run(text.len());
    f32::from(
        window
            .text_system()
            .layout_line(text, px(TEXT_PX), &[run], None)
            .width,
    )
}
