//! Statistics of a session's latest completed turn, as the work status card
//! shows them (`getLatestCompletedTurnStats` in `telemetry.ts`).
//!
//! Every value is computed from the transcript: step windows, token counts,
//! costs and the times of text and tool parts. A value whose inputs are
//! missing or implausible is `None`, never zero.

use crate::transcript::{Message, PartContent, PartTime, Role, ToolStatus, Usd};

/// No provider streams faster; a higher rate means the window is broken.
const MAX_PLAUSIBLE_TOKENS_PER_SECOND: f64 = 5_000.0;

/// The latest completed turn's statistics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TurnStats {
    pub steps: usize,
    pub elapsed_ms: Option<i64>,
    pub model_ms: Option<i64>,
    pub tool_ms: Option<i64>,
    pub average_ttft_ms: Option<f64>,
    /// Output and reasoning tokens over model time.
    pub tokens_per_second: Option<f64>,
    /// The final reply's text delivery rate.
    pub response_tokens_per_second: Option<f64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub generated_tokens: Option<u64>,
    pub cache_hit_percent: Option<u64>,
    pub cost: Option<Usd>,
}

/// One completed step's contribution.
struct StepStats {
    tool_ms: Option<i64>,
    model_ms: Option<i64>,
    ttft_ms: Option<i64>,
    input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    cost: Option<Usd>,
}

/// The newest user-bounded turn's statistics, or `None` when the newest
/// conversation record is not a finished assistant step of a turn whose
/// steps all completed.
pub fn latest_completed_turn(messages: &[Message]) -> Option<TurnStats> {
    let conversation =
        |message: &Message| matches!(message.info.role, Role::User | Role::Assistant);
    let last = messages.iter().rposition(conversation)?;
    let final_step = &messages[last];
    if final_step.info.role != Role::Assistant {
        return None;
    }
    let prompt = messages
        .iter()
        .rposition(|message| message.info.role == Role::User)?;
    let turn_start = Some(messages[prompt].info.created).filter(|created| *created >= 0);
    let turn_end = final_step
        .info
        .completed
        .filter(|completed| *completed >= 0);
    let elapsed_ms = turn_start
        .zip(turn_end)
        .filter(|(start, end)| end >= start)
        .map(|(start, end)| end - start);
    let mut steps = Vec::new();
    for message in &messages[prompt + 1..=last] {
        if message.info.role == Role::Assistant {
            steps.push(step_stats(message)?);
        }
    }
    if steps.is_empty() {
        return None;
    }
    let sum = |value: fn(&StepStats) -> Option<i64>| -> Option<i64> {
        steps
            .iter()
            .try_fold(0_i64, |total, step| Some(total + value(step)?))
    };
    let sum_tokens = |value: fn(&StepStats) -> Option<u64>| -> Option<u64> {
        steps
            .iter()
            .try_fold(0_u64, |total, step| Some(total + value(step)?))
    };
    let model_ms = sum(|step| step.model_ms);
    let tool_ms = sum(|step| step.tool_ms);
    let ttft_total = sum(|step| step.ttft_ms);
    let input = sum_tokens(|step| step.input);
    let output = sum_tokens(|step| step.output);
    let reasoning = sum_tokens(|step| step.reasoning);
    let cache_read = sum_tokens(|step| step.cache_read);
    let cache_write = sum_tokens(|step| step.cache_write);
    let cost = steps
        .iter()
        .try_fold(Usd::default(), |total, step| Some(total + step.cost?));
    let generated = output
        .zip(reasoning)
        .map(|(output, reasoning)| output + reasoning);
    let tokens_per_second = generated
        .zip(model_ms)
        .and_then(|(tokens, duration)| token_rate(tokens, duration));
    let cache_hit_percent = match (input, cache_read, cache_write) {
        (Some(input), Some(read), Some(write)) if input + read + write > 0 => {
            let percent = (read as f64 / (input + read + write) as f64 * 100.0).clamp(0.0, 100.0);
            Some(percent.round() as u64)
        }
        _ => None,
    };
    Some(TurnStats {
        steps: steps.len(),
        elapsed_ms,
        model_ms,
        tool_ms,
        average_ttft_ms: ttft_total.map(|total| total as f64 / steps.len() as f64),
        tokens_per_second,
        response_tokens_per_second: response_rate(final_step),
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: reasoning,
        generated_tokens: generated,
        cache_hit_percent,
        cost,
    })
}

/// A finished step's durations, tokens and cost; `None` while it runs.
fn step_stats(message: &Message) -> Option<StepStats> {
    let created = message.info.created;
    let completed = message.info.completed?;
    let window = created >= 0 && completed >= created;
    let mut tools_valid = window;
    let mut tool_intervals = Vec::new();
    let mut ttft_ms: Option<i64> = None;
    for part in &message.parts {
        match &part.content {
            PartContent::Tool { status, .. } => {
                let finished = matches!(status, ToolStatus::Completed | ToolStatus::Error);
                match part.time {
                    PartTime {
                        start: Some(start),
                        end: Some(end),
                    } if finished && start >= created && end <= completed && end >= start => {
                        tool_intervals.push((start, end));
                    }
                    _ => tools_valid = false,
                }
            }
            // A start equal to the step's creation is the creation stamp
            // copied onto the part, not a measured first token.
            PartContent::Text { .. } | PartContent::Reasoning { .. } => {
                if let Some(start) = part.time.start
                    && window
                    && start > created
                    && start <= completed
                {
                    let delay = start - created;
                    ttft_ms = Some(ttft_ms.map_or(delay, |earliest| earliest.min(delay)));
                }
            }
            _ => {}
        }
    }
    let tool_ms = tools_valid.then(|| merged_duration(tool_intervals));
    let model_ms = tool_ms
        .filter(|_| window)
        .map(|tools| completed - created - tools)
        .filter(|model| *model >= 0);
    let facts = message.info.assistant.as_ref();
    let tokens = facts.and_then(|facts| facts.outcome.tokens);
    Some(StepStats {
        tool_ms,
        model_ms,
        ttft_ms,
        input: tokens.map(|tokens| tokens.input),
        output: tokens.map(|tokens| tokens.output),
        reasoning: tokens.map(|tokens| tokens.reasoning),
        cache_read: tokens.map(|tokens| tokens.cache_read),
        cache_write: tokens.map(|tokens| tokens.cache_write),
        cost: facts.and_then(|facts| facts.outcome.cost),
    })
}

/// The final reply's output over its merged text intervals; `None` for a
/// step that failed, ran tools, or whose text times fall outside its window.
/// A history read stamps text with the step's own creation, which is no
/// measured first token: dividing by it would count the initial wait, so a
/// start at the creation leaves the rate unknown, as after a reload on the
/// web.
fn response_rate(message: &Message) -> Option<f64> {
    let facts = message.info.assistant.as_ref()?;
    if facts.outcome.error.is_some()
        || message
            .parts
            .iter()
            .any(|part| matches!(part.content, PartContent::Tool { .. }))
    {
        return None;
    }
    let output = facts.outcome.tokens?.output;
    let created = message.info.created;
    let completed = message.info.completed?;
    if created < 0 || completed < 0 {
        return None;
    }
    let mut intervals = Vec::new();
    for part in &message.parts {
        let PartContent::Text { text } = &part.content else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let (Some(start), Some(end)) = (part.time.start, part.time.end) else {
            return None;
        };
        if start <= created || end > completed || end <= start {
            return None;
        }
        intervals.push((start, end));
    }
    token_rate(output, merged_duration(intervals))
}

fn token_rate(tokens: u64, duration_ms: i64) -> Option<f64> {
    if duration_ms <= 0 {
        return None;
    }
    let rate = tokens as f64 / (duration_ms as f64 / 1000.0);
    (0.0..=MAX_PLAUSIBLE_TOKENS_PER_SECOND)
        .contains(&rate)
        .then_some(rate)
}

/// The length of the union of `intervals`, counting overlaps once.
fn merged_duration(mut intervals: Vec<(i64, i64)>) -> i64 {
    intervals.retain(|(start, end)| end >= start);
    intervals.sort_unstable();
    let mut total = 0;
    let mut current: Option<(i64, i64)> = None;
    for (start, end) in intervals {
        current = match current {
            Some((open, close)) if start <= close => Some((open, close.max(end))),
            Some((open, close)) => {
                total += close - open;
                Some((start, end))
            }
            None => Some((start, end)),
        };
    }
    total + current.map_or(0, |(open, close)| close - open)
}

/// `formatThroughputRate`: `~4122 tok/s`.
pub fn format_rate(tokens_per_second: f64) -> String {
    format!("~{} tok/s", js_round(tokens_per_second))
}

/// `formatTelemetryDuration`: tenths of a second under a minute, whole
/// seconds after, `<0.1s` for a real wait that rounds to zero.
pub fn format_duration(ms: f64) -> String {
    if !ms.is_finite() || ms <= 0.0 {
        return "0.0s".to_owned();
    }
    if ms < 50.0 {
        return "<0.1s".to_owned();
    }
    if ms < 60_000.0 {
        return format!("{}s", to_fixed_1(ms / 1000.0));
    }
    let total_seconds = js_round(ms / 1000.0);
    format!("{}m {}s", total_seconds / 60, total_seconds % 60)
}

/// `formatTelemetryTokens`: `2.3K`, `1.0M`, or the count.
pub fn format_tokens(tokens: u64) -> String {
    let tokens = tokens as f64;
    if tokens <= 0.0 {
        "0".to_owned()
    } else if tokens >= 1_000_000.0 {
        format!("{}M", to_fixed_1(tokens / 1_000_000.0))
    } else if tokens >= 1_000.0 {
        format!("{}K", to_fixed_1(tokens / 1_000.0))
    } else {
        js_round(tokens).to_string()
    }
}

/// The card's cost: three decimals without trailing zeros, `$0` for none.
pub fn format_cost(cost: Usd) -> String {
    let fixed = format!("{:.3}", js_round_to(cost.dollars(), 3));
    let trimmed = fixed.trim_end_matches('0').trim_end_matches('.');
    format!("${trimmed}")
}

/// `Math.round`: halves round up.
fn js_round(value: f64) -> i64 {
    (value + 0.5).floor() as i64
}

/// `toFixed(1)`, whose exact halves round up.
fn to_fixed_1(value: f64) -> String {
    format!("{:.1}", js_round_to(value, 1))
}

fn js_round_to(value: f64, digits: i32) -> f64 {
    let scale = 10_f64.powi(digits);
    let scaled = value * scale;
    if scaled.fract() == 0.5 {
        scaled.ceil() / scale
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{MessageId, PartId};
    use crate::transcript::{AssistantFacts, MessageInfo, Part, StepOutcome, TokenUsage};

    fn user(created: i64) -> Message {
        Message {
            info: MessageInfo {
                id: MessageId::from(format!("u{created}").as_str()),
                role: Role::User,
                created,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
            },
            parts: vec![Part::text("u-text", "go")],
        }
    }

    fn step(created: i64, completed: Option<i64>, output: u64, parts: Vec<Part>) -> Message {
        Message {
            info: MessageInfo {
                id: MessageId::from(format!("a{created}").as_str()),
                role: Role::Assistant,
                created,
                completed,
                notice: None,
                assistant: Some(AssistantFacts {
                    outcome: StepOutcome {
                        finish: Some("stop".to_owned()),
                        tokens: Some(TokenUsage {
                            input: 1,
                            output,
                            ..TokenUsage::default()
                        }),
                        cost: Usd::from_dollars(0.0),
                        ..StepOutcome::default()
                    },
                    ..AssistantFacts::default()
                }),
                compaction: None,
            },
            parts,
        }
    }

    fn text(start: i64, end: i64) -> Part {
        Part {
            id: PartId::from("text"),
            content: PartContent::Text {
                text: "answer".to_owned(),
            },
            time: PartTime {
                start: Some(start),
                end: Some(end),
            },
        }
    }

    fn tool(start: i64, end: i64) -> Part {
        Part {
            id: PartId::from("tool"),
            content: PartContent::Tool {
                tool: "read".to_owned(),
                status: ToolStatus::Completed,
                title: None,
                output: None,
                elided: None,
            },
            time: PartTime {
                start: Some(start),
                end: Some(end),
            },
        }
    }

    /// The reference's `parity: long` turn read from history: one 0.55 s
    /// text step 0.6 s after the prompt, its text stamped with the step's
    /// creation.
    #[test]
    fn a_text_only_turn_reads_as_the_reference() {
        let messages = [
            user(1_000),
            step(1_049, Some(1_600), 2_263, vec![text(1_049, 1_600)]),
        ];
        let stats = latest_completed_turn(&messages).expect("a completed turn");
        assert_eq!(stats.elapsed_ms, Some(600));
        assert_eq!(stats.tool_ms, Some(0));
        assert_eq!(stats.model_ms, Some(551));
        assert_eq!(
            stats.average_ttft_ms, None,
            "the creation stamp is no first token"
        );
        assert_eq!(
            stats.response_tokens_per_second, None,
            "a creation stamp measures no response"
        );
        assert_eq!(
            format_rate(stats.tokens_per_second.expect("measured")),
            "~4107 tok/s"
        );
        assert_eq!(
            format_duration(stats.elapsed_ms.expect("known") as f64),
            "0.6s"
        );
        assert_eq!(
            (
                format_tokens(stats.input_tokens.expect("known")),
                format_tokens(stats.generated_tokens.expect("known"))
            ),
            ("1".to_owned(), "2.3K".to_owned())
        );
        assert_eq!(stats.cache_hit_percent, Some(0));
        assert_eq!(format_cost(stats.cost.expect("known")), "$0");
    }

    #[test]
    fn tools_take_their_merged_time_from_the_model() {
        let messages = [
            user(0),
            step(100, Some(1_100), 400, vec![tool(200, 600), tool(500, 700)]),
            step(1_200, Some(2_200), 600, vec![text(1_300, 2_200)]),
        ];
        let stats = latest_completed_turn(&messages).expect("a completed turn");
        assert_eq!(stats.steps, 2);
        assert_eq!(stats.tool_ms, Some(500), "overlapping calls count once");
        assert_eq!(stats.model_ms, Some(1_500));
        assert_eq!(
            stats.average_ttft_ms, None,
            "the tool step reported no first token"
        );
        assert_eq!(
            format_rate(stats.tokens_per_second.expect("measured")),
            "~667 tok/s"
        );
    }

    #[test]
    fn streamed_text_times_measure_the_response() {
        let messages = [user(0), step(100, Some(1_100), 600, vec![text(400, 1_000)])];
        let stats = latest_completed_turn(&messages).expect("a completed turn");
        assert_eq!(
            format_rate(stats.response_tokens_per_second.expect("measured")),
            "~1000 tok/s"
        );
    }

    #[test]
    fn an_unfinished_or_trailing_prompt_turn_has_no_stats() {
        assert_eq!(
            latest_completed_turn(&[user(0), step(10, None, 5, vec![])]),
            None
        );
        assert_eq!(
            latest_completed_turn(&[user(0), step(10, Some(20), 5, vec![]), user(30)]),
            None
        );
    }

    #[test]
    fn formats_follow_the_web() {
        assert_eq!(format_duration(49.0), "<0.1s");
        assert_eq!(format_duration(0.0), "0.0s");
        assert_eq!(format_duration(62_400.0), "1m 2s");
        assert_eq!(format_tokens(1_000), "1.0K");
        assert_eq!(format_tokens(2_500_000), "2.5M");
        assert_eq!(
            format_cost(Usd::from_dollars(0.12345).expect("valid")),
            "$0.123"
        );
        assert_eq!(format_cost(Usd::from_dollars(1.5).expect("valid")), "$1.5");
    }
}
