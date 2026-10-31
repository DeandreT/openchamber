//! Where fork, staged revert and `/btw` cut a loaded transcript, as
//! `session-actions.ts`, `revertedMessageDockState.ts` and `btw.ts` decide.
//!
//! Every decision reads messages in server order. Identifier spelling never
//! establishes chronology.

use crate::ids::MessageId;
use crate::transcript::{Message, PartContent, Role};

/// Where a revert or fork of `id` cuts the transcript
/// (`transcriptCutForMessage`): a prompt's context arrives as the synthetic
/// records right before it, so the cut starts at the first of them and they
/// leave with the prompt. Any other target, such as a subagent report, is cut
/// at itself.
pub fn transcript_cut(messages: &[Message], id: &MessageId) -> MessageId {
    let Some(index) = messages.iter().position(|message| &message.info.id == id) else {
        return id.clone();
    };
    if messages[index].info.role != Role::User {
        return id.clone();
    }
    let mut first = index;
    while first > 0 && messages[first - 1].info.role == Role::Synthetic {
        first -= 1;
    }
    messages[first].info.id.clone()
}

/// How many leading messages a staged revert leaves visible (`messagesBefore`).
/// A marker the transcript does not hold hides nothing.
pub fn visible_len(messages: &[Message], revert: Option<&MessageId>) -> usize {
    revert
        .and_then(|marker| {
            messages
                .iter()
                .position(|message| &message.info.id == marker)
        })
        .unwrap_or(messages.len())
}

/// What one reverted entry shows in the strip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertedEntry {
    pub id: MessageId,
    /// The one-line preview; `None` when the entry has nothing to show and the
    /// caller uses its "No text content" label.
    pub preview: Option<String>,
}

/// The prompts and subagent reports a staged revert hides, in order
/// (`buildRevertedMessageDockState` and `RevertedMessagesStrip`). Empty when
/// there is no marker or the transcript does not hold it.
pub fn reverted_entries(messages: &[Message], revert: Option<&MessageId>) -> Vec<RevertedEntry> {
    let Some(marker) = revert else {
        return Vec::new();
    };
    let Some(start) = messages
        .iter()
        .position(|message| &message.info.id == marker)
    else {
        return Vec::new();
    };
    messages[start..]
        .iter()
        .filter_map(|message| match message.info.role {
            Role::User => Some(RevertedEntry {
                id: message.info.id.clone(),
                preview: prompt_preview(message),
            }),
            Role::Synthetic => {
                let facts = message.info.synthetic.as_ref()?;
                facts.subagent.as_ref()?;
                Some(RevertedEntry {
                    id: message.info.id.clone(),
                    preview: facts
                        .description
                        .as_deref()
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned),
                })
            }
            _ => None,
        })
        .collect()
}

/// A prompt's one-line preview: its text with whitespace collapsed, else its
/// first attachment's name in brackets.
fn prompt_preview(message: &Message) -> Option<String> {
    let text = message
        .parts
        .iter()
        .filter_map(|part| match &part.content {
            PartContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    if !text.is_empty() {
        return Some(text);
    }
    message.parts.iter().find_map(|part| match &part.content {
        PartContent::File { filename, .. } if !filename.is_empty() => Some(format!("[{filename}]")),
        _ => None,
    })
}

/// The text a revert or fork puts back in the composer: the prompt's text
/// parts joined by newlines and trimmed. Only a prompt has text to restore.
pub fn restored_prompt_text(message: &Message) -> Option<String> {
    if message.info.role != Role::User {
        return None;
    }
    let text = message
        .parts
        .iter()
        .filter_map(|part| match &part.content {
            PartContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// A record that starts something new after a turn (`isTurnBoundary`): a
/// prompt, a compaction, a shell run, a background subagent run report, a
/// dispatched session's result or a workflow report.
pub fn is_turn_boundary(message: &Message) -> bool {
    matches!(
        message.info.role,
        Role::User | Role::Compaction | Role::Shell
    ) || message.info.synthetic.as_ref().is_some_and(|facts| {
        facts.subagent.is_some()
            || facts.dispatched_session.is_some()
            || facts.workflow_run.is_some()
    })
}

/// Where a fork that keeps the turn through `id` cuts (`forkAfterMessage`):
/// before the first boundary after it, or nowhere when none follows, which
/// copies the whole transcript. `None` when `id` is not loaded.
pub fn fork_after_cut(messages: &[Message], id: &MessageId) -> Option<Option<MessageId>> {
    let index = messages.iter().position(|message| &message.info.id == id)?;
    Some(
        messages[index + 1..]
            .iter()
            .find(|message| is_turn_boundary(message))
            .map(|boundary| transcript_cut(messages, &boundary.info.id)),
    )
}

/// Whether the boundary at `index` opened a turn (`opensTurn`): the assistant
/// step before it finished the previous turn, or there is none.
fn opens_turn(messages: &[Message], index: usize) -> bool {
    messages[..index]
        .iter()
        .rev()
        .find(|message| message.info.role == Role::Assistant)
        .is_none_or(|assistant| {
            assistant.info.completed.is_some() && finish(assistant) != Some("tool-calls")
        })
}

fn finish(message: &Message) -> Option<&str> {
    message
        .info
        .assistant
        .as_ref()
        .and_then(|facts| facts.outcome.finish.as_deref())
}

/// The last assistant message of the last finished turn
/// (`findLastCompletedTurnMessageId`). While a turn runs, everything from the
/// boundary that opened it on is left out.
pub fn last_completed_turn(messages: &[Message], turn_running: bool) -> Option<MessageId> {
    let end = if turn_running {
        (0..messages.len())
            .rev()
            .find(|&index| is_turn_boundary(&messages[index]) && opens_turn(messages, index))
            .unwrap_or(messages.len())
    } else {
        messages.len()
    };
    messages[..end]
        .iter()
        .rev()
        .find(|message| message.info.role == Role::Assistant && message.info.completed.is_some())
        .map(|message| message.info.id.clone())
}

/// Where `/btw` forks (`findLastCompletedAssistantMessageID`): the newest
/// assistant step that finished with `stop`, so a side question typed mid-turn
/// does not inherit a half-finished one.
pub fn btw_fork_point(messages: &[Message]) -> Option<MessageId> {
    messages
        .iter()
        .rev()
        .find(|message| {
            message.info.role == Role::Assistant
                && message.info.completed.is_some()
                && finish(message) == Some("stop")
        })
        .map(|message| message.info.id.clone())
}

/// The messages a `/btw` fork added after what it inherited
/// (`filterBtwTailMessages`). A boundary the transcript does not hold is
/// outside the loaded suffix, so everything shows.
pub fn btw_tail<'a>(messages: &'a [Message], boundary: Option<&MessageId>) -> &'a [Message] {
    let Some(boundary) = boundary else {
        return messages;
    };
    match messages
        .iter()
        .position(|message| &message.info.id == boundary)
    {
        Some(index) => &messages[index + 1..],
        None => messages,
    }
}

/// From when a cascading revert reaches descendants
/// (`descendantRevertCutoff`): a subagent report lands after its child already
/// worked, so reverting it reverts the child from its start.
pub fn descendant_cutoff(target: &Message, child_created: Option<i64>) -> i64 {
    match child_created {
        Some(created) if target_child(target).is_some() => created.min(target.info.created),
        _ => target.info.created,
    }
}

/// The child session a subagent report names.
pub fn target_child(message: &Message) -> Option<&crate::ids::SessionId> {
    message
        .info
        .synthetic
        .as_ref()
        .and_then(|facts| facts.subagent.as_ref())
        .map(|run| &run.child)
}

/// The earliest prompt created at or after `cutoff` (`firstUserMessageAtOrAfter`).
/// Equal times belong to the reverted side.
pub fn first_user_at_or_after(messages: &[Message], cutoff: i64) -> Option<&MessageId> {
    messages
        .iter()
        .filter(|message| message.info.role == Role::User && message.info.created >= cutoff)
        .min_by_key(|message| message.info.created)
        .map(|message| &message.info.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::SessionId;
    use crate::transcript::{
        AssistantFacts, MessageInfo, Part, StepOutcome, SubagentRun, SubagentRunState,
        SyntheticFacts,
    };

    fn message(id: &str, role: Role, created: i64) -> Message {
        Message {
            info: MessageInfo {
                id: MessageId::from(id),
                role,
                created,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
                synthetic: None,
            },
            parts: Vec::new(),
        }
    }

    fn user(id: &str, text: &str) -> Message {
        let mut prompt = message(id, Role::User, 0);
        prompt.parts.push(Part::text(format!("{id}:text"), text));
        prompt
    }

    fn assistant(id: &str, finish: Option<&str>) -> Message {
        let mut step = message(id, Role::Assistant, 0);
        step.info.completed = finish.map(|_| 1);
        step.info.assistant = Some(AssistantFacts {
            outcome: StepOutcome {
                finish: finish.map(str::to_owned),
                ..StepOutcome::default()
            },
            ..AssistantFacts::default()
        });
        step
    }

    fn report(id: &str, child: &str, description: Option<&str>) -> Message {
        let mut record = message(id, Role::Synthetic, 0);
        record.info.synthetic = Some(Box::new(SyntheticFacts {
            description: description.map(str::to_owned),
            subagent: Some(SubagentRun {
                child: SessionId::from(child),
                agent: None,
                state: SubagentRunState::Completed,
            }),
            ..SyntheticFacts::default()
        }));
        record
    }

    fn ids(messages: &[Message]) -> Vec<&str> {
        messages
            .iter()
            .map(|message| message.info.id.as_str())
            .collect()
    }

    #[test]
    fn a_prompt_is_cut_with_its_leading_context() {
        let messages = [
            user("u1", "one"),
            assistant("a1", Some("stop")),
            message("ctx1", Role::Synthetic, 0),
            message("ctx2", Role::Synthetic, 0),
            user("u2", "two"),
            report("r1", "child", None),
        ];
        assert_eq!(transcript_cut(&messages, &"u2".into()).as_str(), "ctx1");
        assert_eq!(transcript_cut(&messages, &"u1".into()).as_str(), "u1");
        assert_eq!(
            transcript_cut(&messages, &"r1".into()).as_str(),
            "r1",
            "a report is cut at itself"
        );
        assert_eq!(transcript_cut(&messages, &"gone".into()).as_str(), "gone");
    }

    #[test]
    fn the_marker_hides_itself_and_what_follows() {
        let messages = [
            user("u1", "a"),
            assistant("a1", Some("stop")),
            user("u2", "b"),
        ];
        assert_eq!(visible_len(&messages, Some(&"a1".into())), 1);
        assert_eq!(visible_len(&messages, None), 3);
        assert_eq!(
            visible_len(&messages, Some(&"older".into())),
            3,
            "a marker outside the loaded window hides nothing"
        );
    }

    #[test]
    fn the_strip_lists_prompts_and_reports_after_the_marker() {
        let mut file_only = message("u3", Role::User, 0);
        file_only.parts.push(Part {
            id: "u3:file:0".into(),
            content: PartContent::File {
                filename: "plan.md".to_owned(),
                mime: "text/markdown".to_owned(),
            },
            time: Default::default(),
        });
        let messages = [
            user("u1", "kept"),
            message("ctx", Role::Synthetic, 0),
            user("u2", "  first\n line "),
            assistant("a2", Some("stop")),
            file_only,
            report("r1", "child", Some(" Explore ")),
            message("u4", Role::User, 0),
        ];
        let entries = reverted_entries(&messages, Some(&"ctx".into()));
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.id.as_str(), entry.preview.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("u2", Some("first line")),
                ("u3", Some("[plan.md]")),
                ("r1", Some("Explore")),
                ("u4", None),
            ]
        );
        assert!(reverted_entries(&messages, None).is_empty());
        assert!(reverted_entries(&messages, Some(&"missing".into())).is_empty());
    }

    #[test]
    fn only_a_prompt_restores_text() {
        let mut prompt = user("u1", " hello ");
        prompt.parts.push(Part::text("u1:text:1", "world"));
        assert_eq!(
            restored_prompt_text(&prompt).as_deref(),
            Some("hello \nworld")
        );
        assert_eq!(restored_prompt_text(&report("r", "c", Some("x"))), None);
        assert_eq!(restored_prompt_text(&user("u2", "  ")), None);
    }

    #[test]
    fn forking_after_a_turn_cuts_before_the_next_boundary() {
        let messages = [
            user("u1", "a"),
            assistant("a1", Some("stop")),
            message("note", Role::Synthetic, 0),
            message("ctx", Role::Synthetic, 0),
            user("u2", "b"),
            assistant("a2", Some("stop")),
        ];
        assert_eq!(
            fork_after_cut(&messages, &"a1".into()),
            Some(Some("note".into())),
            "the prompt's context leaves with it"
        );
        assert_eq!(fork_after_cut(&messages, &"a2".into()), Some(None));
        assert_eq!(fork_after_cut(&messages, &"missing".into()), None);
        let with_report = [assistant("a1", Some("stop")), report("r1", "c", None)];
        assert_eq!(
            fork_after_cut(&with_report, &"a1".into()),
            Some(Some("r1".into()))
        );
    }

    #[test]
    fn dispatched_results_and_workflow_reports_are_turn_boundaries() {
        let report = |id: &str, facts: SyntheticFacts| {
            let mut record = message(id, Role::Synthetic, 0);
            record.info.synthetic = Some(Box::new(facts));
            record
        };
        for boundary in [
            report(
                "result",
                SyntheticFacts {
                    dispatched_session: Some(SessionId::from("ses_d")),
                    ..SyntheticFacts::default()
                },
            ),
            report(
                "workflow",
                SyntheticFacts {
                    workflow_run: Some("run_1".to_owned()),
                    ..SyntheticFacts::default()
                },
            ),
        ] {
            let id = boundary.info.id.clone();
            let messages = [assistant("a1", Some("stop")), boundary];
            assert_eq!(
                fork_after_cut(&messages, &"a1".into()),
                Some(Some(id.clone())),
                "{id} starts something new"
            );
        }
        let context = report("ctx", SyntheticFacts::default());
        assert!(
            !is_turn_boundary(&context),
            "plain context is not a boundary"
        );
    }

    #[test]
    fn the_last_completed_turn_skips_a_running_one() {
        let messages = [
            user("u1", "a"),
            assistant("a1", Some("stop")),
            user("u2", "b"),
            assistant("a2", Some("tool-calls")),
            // A prompt steered into the running turn does not open one.
            user("u3", "steer"),
            assistant("a3", None),
        ];
        assert_eq!(
            last_completed_turn(&messages, true)
                .as_ref()
                .map(MessageId::as_str),
            Some("a1")
        );
        assert_eq!(
            last_completed_turn(&messages, false)
                .as_ref()
                .map(MessageId::as_str),
            Some("a2")
        );
        assert_eq!(last_completed_turn(&[user("u", "x")], false), None);
    }

    #[test]
    fn btw_forks_at_the_last_stopped_step_and_shows_its_tail() {
        let messages = [
            user("u1", "a"),
            assistant("a1", Some("stop")),
            user("u2", "b"),
            assistant("a2", Some("tool-calls")),
        ];
        assert_eq!(btw_fork_point(&messages), Some("a1".into()));
        assert_eq!(btw_fork_point(&messages[..1]), None);
        assert_eq!(ids(btw_tail(&messages, Some(&"a1".into()))), ["u2", "a2"]);
        assert_eq!(ids(btw_tail(&messages, None)).len(), 4);
        assert_eq!(ids(btw_tail(&messages, Some(&"older".into()))).len(), 4);
    }

    #[test]
    fn descendants_revert_from_the_child_start_for_a_report() {
        let mut target = report("r1", "child", None);
        target.info.created = 50;
        assert_eq!(descendant_cutoff(&target, Some(20)), 20);
        assert_eq!(descendant_cutoff(&target, None), 50);
        let mut prompt = user("u", "x");
        prompt.info.created = 40;
        assert_eq!(descendant_cutoff(&prompt, Some(10)), 40);
        let mut early = user("e", "x");
        early.info.created = 5;
        let mut equal = user("q", "x");
        equal.info.created = 20;
        let mut later = user("l", "x");
        later.info.created = 30;
        let child = [early, later, equal];
        assert_eq!(
            first_user_at_or_after(&child, 20).map(MessageId::as_str),
            Some("q"),
            "equal times belong to the reverted side"
        );
        assert_eq!(first_user_at_or_after(&child, 31), None);
    }
}
