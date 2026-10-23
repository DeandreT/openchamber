//! How full a session's context window is, as the work status card reads it
//! (`contextUsage.ts` over `findLatestContextFill` in `tokenUtils.ts`).

use crate::transcript::{CompactionStatus, Message, Role};

/// The limit used when the model reports none.
pub const DEFAULT_CONTEXT_LIMIT: u64 = 200_000;

/// The context window's fill.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ContextUsage {
    /// The newest response that reported tokens says how much the window
    /// holds; `percent` is unrounded.
    Measured {
        tokens: u64,
        limit: u64,
        percent: f64,
    },
    /// A finished compaction is newer than any such response: the fill is
    /// unknown until the next one, not zero.
    Compacted { limit: u64 },
}

/// The tone of a reading, from its rounded percentage (`resolveUsageTone`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageTone {
    Safe,
    Warn,
    Critical,
}

/// The fill read from the newest message that can say, or `None` before any
/// response reported tokens. A running or failed compaction has not changed
/// the window and is passed over. `context_limit` is the model's limit; an
/// unknown or zero limit uses [`DEFAULT_CONTEXT_LIMIT`].
pub fn context_usage(messages: &[Message], context_limit: Option<u64>) -> Option<ContextUsage> {
    let limit = context_limit
        .filter(|limit| *limit > 0)
        .unwrap_or(DEFAULT_CONTEXT_LIMIT);
    for message in messages.iter().rev() {
        match message.info.role {
            Role::Compaction if message.info.compaction == Some(CompactionStatus::Completed) => {
                return Some(ContextUsage::Compacted { limit });
            }
            Role::Assistant => {
                let tokens = message
                    .info
                    .assistant
                    .as_ref()
                    .and_then(|facts| facts.outcome.tokens)
                    .map_or(0, |tokens| tokens.total());
                if tokens > 0 {
                    return Some(ContextUsage::Measured {
                        tokens,
                        limit,
                        percent: tokens as f64 / limit as f64 * 100.0,
                    });
                }
            }
            _ => {}
        }
    }
    None
}

/// `min(percent, 999).toFixed(1)` and a percent sign. `toFixed` rounds an
/// exact half up, where Rust's formatting rounds it to even.
pub fn format_percent(percent: f64) -> String {
    let capped = percent.min(999.0);
    let tenths = capped * 10.0;
    let rounded = if tenths.fract() == 0.5 {
        tenths.ceil() / 10.0
    } else {
        capped
    };
    format!("{rounded:.1}%")
}

impl UsageTone {
    pub fn of(percent: f64) -> Self {
        let rounded = percent.round();
        if rounded >= 80.0 {
            Self::Critical
        } else if rounded >= 50.0 {
            Self::Warn
        } else {
            Self::Safe
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::MessageId;
    use crate::transcript::{AssistantFacts, MessageInfo, StepOutcome, TokenUsage};

    fn record(id: &str, role: Role) -> Message {
        Message {
            info: MessageInfo {
                id: MessageId::from(id),
                role,
                created: 0,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
            },
            parts: Vec::new(),
        }
    }

    fn answer(id: &str, output: u64) -> Message {
        let mut message = record(id, Role::Assistant);
        message.info.assistant = Some(AssistantFacts {
            outcome: StepOutcome {
                tokens: Some(TokenUsage {
                    input: 1,
                    output,
                    ..TokenUsage::default()
                }),
                ..StepOutcome::default()
            },
            ..AssistantFacts::default()
        });
        message
    }

    fn compaction(id: &str, status: CompactionStatus) -> Message {
        let mut message = record(id, Role::Compaction);
        message.info.compaction = Some(status);
        message
    }

    #[test]
    fn the_newest_reading_measures_the_window() {
        let messages = [
            answer("a1", 999),
            record("u1", Role::User),
            answer("a2", 2_263),
            record("i1", Role::Idle),
        ];
        let Some(ContextUsage::Measured {
            tokens,
            limit,
            percent,
        }) = context_usage(&messages, None)
        else {
            panic!("a measured reading");
        };
        assert_eq!((tokens, limit), (2_264, DEFAULT_CONTEXT_LIMIT));
        assert_eq!(format_percent(percent), "1.1%", "the reference's reading");
        assert_eq!(
            context_usage(&messages, Some(0)),
            context_usage(&messages, None),
            "a zero limit is no limit"
        );
    }

    #[test]
    fn a_finished_compaction_makes_the_fill_unknown() {
        let compacted = [
            answer("a1", 5_000),
            compaction("c1", CompactionStatus::Completed),
        ];
        assert_eq!(
            context_usage(&compacted, Some(100_000)),
            Some(ContextUsage::Compacted { limit: 100_000 })
        );
        let unfinished = [
            answer("a1", 4_999),
            compaction("c1", CompactionStatus::Failed),
        ];
        assert!(matches!(
            context_usage(&unfinished, Some(100_000)),
            Some(ContextUsage::Measured { tokens: 5_000, .. })
        ));
        assert_eq!(context_usage(&[record("u1", Role::User)], None), None);
    }

    #[test]
    fn percents_and_tones_follow_the_web() {
        assert_eq!(format_percent(0.25), "0.3%", "an exact half rounds up");
        assert_eq!(format_percent(1234.5), "999.0%");
        assert_eq!(UsageTone::of(49.4), UsageTone::Safe);
        assert_eq!(UsageTone::of(49.5), UsageTone::Warn);
        assert_eq!(UsageTone::of(79.6), UsageTone::Critical);
    }
}
