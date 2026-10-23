//! Ordered session transcripts with stable message and part identities.
//!
//! Server order is authoritative: history arrives in order, live messages
//! append, older pages prepend. Identifier spelling never reorders anything.
//! Every mutation reports which rows changed so the renderer can update one
//! row for a text delta without rebuilding the list.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::ids::{MessageId, PartId};
use crate::send_config::ModelChoice;

/// What a transcript record is.
///
/// Users and assistants carry parts; every other role is one record the
/// transcript shows as a notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    User,
    Assistant,
    /// Text the client or server injected on the user's behalf.
    Synthetic,
    System,
    Skill,
    Shell,
    Compaction,
    AgentSwitched,
    ModelSwitched,
    LocationSwitched,
    /// The end of a turn, with its outcome.
    Idle,
}

impl Role {
    /// Whether records of this role carry parts.
    pub fn has_parts(self) -> bool {
        matches!(self, Self::User | Self::Assistant)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolStatus {
    Pending,
    Running,
    Completed,
    Error,
}

/// What a message part holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartContent {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Tool {
        tool: String,
        status: ToolStatus,
        title: Option<String>,
        output: Option<String>,
        /// Characters of a finished result the server left out of a history
        /// page. The whole message must be fetched to show the result.
        elided: Option<usize>,
    },
    File {
        filename: String,
        mime: String,
    },
    /// An `@agent` mention in a user prompt.
    Agent {
        name: String,
    },
    /// A part type this client does not render; only its type name is kept.
    Unsupported {
        kind: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub id: PartId,
    pub content: PartContent,
}

impl Part {
    pub fn text(id: impl Into<PartId>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: PartContent::Text { text: text.into() },
        }
    }
}

/// Tokens one assistant step used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenUsage {
    /// Every counted token, as `extractTokensFromMessage` sums them when a
    /// reading has no total.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.reasoning + self.cache_read + self.cache_write
    }
}

/// How an assistant step ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StepOutcome {
    /// Why the step ended, such as `stop`.
    pub finish: Option<String>,
    /// The failure's message when the step failed.
    pub error: Option<String>,
    pub tokens: Option<TokenUsage>,
}

/// What an assistant message records about the step that wrote it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssistantFacts {
    pub agent: Option<String>,
    pub model: Option<ModelChoice>,
    pub outcome: StepOutcome,
}

/// Message metadata without its parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageInfo {
    pub id: MessageId,
    pub role: Role,
    pub created: i64,
    /// Present once an assistant message finished.
    pub completed: Option<i64>,
    /// The one line a notice role shows, such as a switched agent's name.
    pub notice: Option<String>,
    /// An assistant message's step facts; `None` for every other role.
    pub assistant: Option<AssistantFacts>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub info: MessageInfo,
    pub parts: Vec<Part>,
}

impl Message {
    /// The concatenated text of the message's text parts, separated by blank lines.
    pub fn plain_text(&self) -> String {
        let texts: Vec<&str> = self
            .parts
            .iter()
            .filter_map(|part| match &part.content {
                PartContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        texts.join("\n\n")
    }
}

/// An assistant row's place in its turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnPosition {
    /// Whether no assistant message follows before the next user message.
    pub last_assistant: bool,
    /// When the user message that opened the turn was created.
    pub prompt_created: Option<i64>,
}

/// A live change to one transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptEvent {
    MessageUpdated(MessageInfo),
    PartUpdated {
        message_id: MessageId,
        part: Part,
    },
    /// Appends to a text or reasoning part.
    PartDelta {
        message_id: MessageId,
        part_id: PartId,
        delta: String,
    },
    MessageRemoved {
        message_id: MessageId,
    },
    PartRemoved {
        message_id: MessageId,
        part_id: PartId,
    },
    /// Replaces every part of a message, such as a queued prompt's.
    PartsReplaced {
        message_id: MessageId,
        parts: Vec<Part>,
    },
    /// A queued prompt reached the agent; its time becomes the delivery time.
    MessageDelivered {
        message_id: MessageId,
        created: i64,
    },
    /// A step ended: the message's completion time is set, its parts kept.
    MessageCompleted {
        message_id: MessageId,
        completed: i64,
        outcome: StepOutcome,
    },
    /// A tool call moved to another state; its name and part identity stay.
    ToolTransition {
        message_id: MessageId,
        part_id: PartId,
        status: ToolStatus,
        output: Option<String>,
    },
}

/// Which rows a mutation affected. Rows are messages in server order.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub enum TranscriptChange {
    /// Nothing observable changed; no notification is due.
    Unchanged,
    /// One row's content changed.
    Updated {
        row: usize,
    },
    Inserted {
        rows: Range<usize>,
    },
    Removed {
        rows: Range<usize>,
    },
    /// The whole transcript was replaced.
    Reset,
    /// The event refers to a message this transcript does not hold, so the
    /// scope needs an authoritative read.
    NeedsReconcile,
}

/// One session's ordered messages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transcript {
    messages: Vec<Message>,
    row_by_id: HashMap<MessageId, usize>,
    revision: u64,
    /// The revision that last changed each message.
    changed_at: HashMap<MessageId, u64>,
    /// Text parts whose next delta may repeat the end of a full snapshot.
    overlapping_delta: HashSet<PartId>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a transcript from complete history in server order.
    pub fn from_history(messages: Vec<Message>) -> Self {
        let mut transcript = Self::new();
        let _ = transcript.replace(messages);
        transcript
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn message(&self, row: usize) -> Option<&Message> {
        self.messages.get(row)
    }

    pub fn row_of(&self, id: &MessageId) -> Option<usize> {
        self.row_by_id.get(id).copied()
    }

    /// Where an assistant row sits in its turn: the user message that opened
    /// the turn and every assistant message up to the next user message, as
    /// `MessageList.tsx` groups turns. Other roles neither open nor close a
    /// turn. `None` for a row that is not an assistant message.
    pub fn turn_position(&self, row: usize) -> Option<TurnPosition> {
        if self.messages.get(row)?.info.role != Role::Assistant {
            return None;
        }
        let last_assistant = self.messages[row + 1..]
            .iter()
            .map(|message| message.info.role)
            .find(|role| matches!(role, Role::User | Role::Assistant))
            != Some(Role::Assistant);
        let prompt_created = self.messages[..row]
            .iter()
            .rev()
            .find(|message| message.info.role == Role::User)
            .map(|message| message.info.created);
        Some(TurnPosition {
            last_assistant,
            prompt_created,
        })
    }

    /// Increments on every observable mutation.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replaces the transcript with authoritative history.
    ///
    /// A history page may carry finished tool results as shells; a whole
    /// result this transcript already holds for the same call and status
    /// stays.
    pub fn replace(&mut self, mut messages: Vec<Message>) -> TranscriptChange {
        for message in &mut messages {
            if let Some(row) = self.row_of(&message.info.id) {
                keep_whole_results(&self.messages[row], message);
            }
        }
        if messages == self.messages {
            return TranscriptChange::Unchanged;
        }
        self.messages = messages;
        self.reindex(0);
        self.revision += 1;
        self.changed_at = self
            .messages
            .iter()
            .map(|message| (message.info.id.clone(), self.revision))
            .collect();
        self.overlapping_delta.clear();
        TranscriptChange::Reset
    }

    /// Prepends an older history page, skipping messages already held.
    pub fn prepend(&mut self, page: Vec<Message>) -> TranscriptChange {
        let fresh: Vec<Message> = page
            .into_iter()
            .filter(|message| !self.row_by_id.contains_key(&message.info.id))
            .collect();
        if fresh.is_empty() {
            return TranscriptChange::Unchanged;
        }
        let count = fresh.len();
        self.messages.splice(0..0, fresh);
        self.reindex(0);
        for row in 0..count {
            self.touch(row);
        }
        TranscriptChange::Inserted { rows: 0..count }
    }

    /// Appends a message, or updates it in place when already held.
    pub fn push(&mut self, mut message: Message) -> TranscriptChange {
        if let Some(row) = self.row_of(&message.info.id) {
            keep_whole_results(&self.messages[row], &mut message);
            if self.messages[row] == message {
                return TranscriptChange::Unchanged;
            }
            self.messages[row] = message;
            self.touch(row);
            return TranscriptChange::Updated { row };
        }
        let row = self.messages.len();
        self.row_by_id.insert(message.info.id.clone(), row);
        self.messages.push(message);
        self.touch(row);
        TranscriptChange::Inserted { rows: row..row + 1 }
    }

    /// Applies one live event.
    pub fn apply(&mut self, event: TranscriptEvent) -> TranscriptChange {
        match event {
            TranscriptEvent::MessageUpdated(info) => match self.row_of(&info.id) {
                Some(row) => {
                    let message = &mut self.messages[row];
                    if message.info == info {
                        return TranscriptChange::Unchanged;
                    }
                    message.info = info;
                    self.touch(row);
                    TranscriptChange::Updated { row }
                }
                None => self.push(Message {
                    info,
                    parts: Vec::new(),
                }),
            },
            TranscriptEvent::PartUpdated { message_id, part } => {
                let Some(row) = self.row_of(&message_id) else {
                    return TranscriptChange::NeedsReconcile;
                };
                let parts = &mut self.messages[row].parts;
                match parts.iter_mut().find(|existing| existing.id == part.id) {
                    Some(existing) if *existing == part => return TranscriptChange::Unchanged,
                    // A finished call never goes back to an active state.
                    Some(existing) if is_final_tool(existing) && is_active_tool(&part) => {
                        return TranscriptChange::Unchanged;
                    }
                    Some(existing) => {
                        if may_repeat_text(existing, &part) {
                            self.overlapping_delta.insert(part.id.clone());
                        }
                        *existing = part;
                    }
                    None => parts.push(part),
                }
                self.touch(row);
                TranscriptChange::Updated { row }
            }
            TranscriptEvent::PartDelta {
                message_id,
                part_id,
                delta,
            } => {
                let Some(row) = self.row_of(&message_id) else {
                    return TranscriptChange::NeedsReconcile;
                };
                if delta.is_empty() {
                    return TranscriptChange::Unchanged;
                }
                let parts = &mut self.messages[row].parts;
                // A delta for a part this transcript never saw started would
                // build text without its beginning; the scope needs a read.
                let Some(Part {
                    content: PartContent::Text { text } | PartContent::Reasoning { text },
                    ..
                }) = parts.iter_mut().find(|part| part.id == part_id)
                else {
                    return TranscriptChange::NeedsReconcile;
                };
                if self.overlapping_delta.remove(&part_id) {
                    let appended = append_without_overlap(text, &delta);
                    if !appended {
                        return TranscriptChange::Unchanged;
                    }
                } else {
                    text.push_str(&delta);
                }
                self.touch(row);
                TranscriptChange::Updated { row }
            }
            TranscriptEvent::MessageRemoved { message_id } => {
                let Some(row) = self.row_of(&message_id) else {
                    return TranscriptChange::Unchanged;
                };
                self.messages.remove(row);
                self.row_by_id.remove(&message_id);
                self.changed_at.remove(&message_id);
                self.reindex(row);
                self.revision += 1;
                TranscriptChange::Removed { rows: row..row + 1 }
            }
            TranscriptEvent::PartsReplaced { message_id, parts } => {
                let Some(row) = self.row_of(&message_id) else {
                    return TranscriptChange::NeedsReconcile;
                };
                if self.messages[row].parts == parts {
                    return TranscriptChange::Unchanged;
                }
                self.messages[row].parts = parts;
                self.touch(row);
                TranscriptChange::Updated { row }
            }
            TranscriptEvent::MessageDelivered {
                message_id,
                created,
            } => {
                let Some(row) = self.row_of(&message_id) else {
                    return TranscriptChange::NeedsReconcile;
                };
                let info = &mut self.messages[row].info;
                if info.created == created {
                    return TranscriptChange::Unchanged;
                }
                info.created = created;
                self.touch(row);
                TranscriptChange::Updated { row }
            }
            TranscriptEvent::MessageCompleted {
                message_id,
                completed,
                outcome,
            } => {
                let Some(row) = self.row_of(&message_id) else {
                    return TranscriptChange::NeedsReconcile;
                };
                let info = &mut self.messages[row].info;
                let facts = info.assistant.get_or_insert_default();
                if info.completed == Some(completed) && facts.outcome == outcome {
                    return TranscriptChange::Unchanged;
                }
                info.completed = Some(completed);
                facts.outcome = outcome;
                self.touch(row);
                TranscriptChange::Updated { row }
            }
            TranscriptEvent::ToolTransition {
                message_id,
                part_id,
                status: next_status,
                output: next_output,
            } => {
                let Some(row) = self.row_of(&message_id) else {
                    return TranscriptChange::NeedsReconcile;
                };
                let part = self.messages[row]
                    .parts
                    .iter_mut()
                    .find(|part| part.id == part_id);
                let Some(Part {
                    content:
                        PartContent::Tool {
                            status,
                            output,
                            elided,
                            ..
                        },
                    ..
                }) = part
                else {
                    return TranscriptChange::NeedsReconcile;
                };
                // A transition with output carries the whole result.
                let next_elided = if next_output.is_some() { None } else { *elided };
                let next_output = next_output.or_else(|| output.clone());
                if *status == next_status && *output == next_output && *elided == next_elided {
                    return TranscriptChange::Unchanged;
                }
                *status = next_status;
                *output = next_output;
                *elided = next_elided;
                self.touch(row);
                TranscriptChange::Updated { row }
            }
            TranscriptEvent::PartRemoved {
                message_id,
                part_id,
            } => {
                let Some(row) = self.row_of(&message_id) else {
                    return TranscriptChange::Unchanged;
                };
                let parts = &mut self.messages[row].parts;
                let before = parts.len();
                parts.retain(|part| part.id != part_id);
                if parts.len() == before {
                    return TranscriptChange::Unchanged;
                }
                self.touch(row);
                TranscriptChange::Updated { row }
            }
        }
    }

    /// Merges a newest-first history read into the transcript.
    ///
    /// Mirrors `materializeSessionSnapshots` in
    /// `packages/ui/src/sync/materialization.ts`. `since` is the revision when
    /// the read was requested: messages changed after it are live and newer
    /// than the read, so they stay and follow the history. Held messages the
    /// read does not list are older pages when they precede its oldest message
    /// and deletions when they follow it.
    pub fn materialize(&mut self, history: Vec<Message>, since: u64) -> TranscriptChange {
        let history_ids: HashSet<&MessageId> =
            history.iter().map(|message| &message.info.id).collect();
        let first_shared = self
            .messages
            .iter()
            .position(|message| history_ids.contains(&message.info.id));
        let is_live = |message: &Message| {
            self.changed_at
                .get(&message.info.id)
                .is_some_and(|revision| *revision > since)
        };
        let mut merged: Vec<Message> = Vec::with_capacity(self.messages.len() + history.len());
        // Older pages before the read.
        for (row, held) in self.messages.iter().enumerate() {
            if history_ids.contains(&held.info.id) || is_live(held) {
                continue;
            }
            if first_shared.is_none_or(|first| row < first) {
                merged.push(held.clone());
            }
        }
        for incoming in history {
            let held = self
                .row_of(&incoming.info.id)
                .map(|row| &self.messages[row]);
            merged.push(match held {
                Some(held) => merge_message(held, incoming),
                None => incoming,
            });
        }
        let listed: HashSet<MessageId> = merged
            .iter()
            .map(|message| message.info.id.clone())
            .collect();
        for held in &self.messages {
            if !listed.contains(&held.info.id) && is_live(held) {
                merged.push(held.clone());
            }
        }
        self.install(merged)
    }

    /// Replaces the messages and reports the narrowest change.
    fn install(&mut self, next: Vec<Message>) -> TranscriptChange {
        if next == self.messages {
            return TranscriptChange::Unchanged;
        }
        let same_prefix = self.messages.len() <= next.len()
            && self
                .messages
                .iter()
                .zip(&next)
                .all(|(held, incoming)| held.info.id == incoming.info.id);
        let changed: Vec<usize> = self
            .messages
            .iter()
            .zip(&next)
            .enumerate()
            .filter(|(_, (held, incoming))| held != incoming)
            .map(|(row, _)| row)
            .collect();
        let old_len = self.messages.len();
        self.messages = next;
        self.reindex(0);
        for row in changed
            .iter()
            .copied()
            .chain(old_len.min(self.messages.len())..self.messages.len())
        {
            self.touch(row);
        }
        if !same_prefix {
            return TranscriptChange::Reset;
        }
        match (changed.first(), changed.last()) {
            (None, _) | (_, None) => TranscriptChange::Inserted {
                rows: old_len..self.messages.len(),
            },
            (Some(&first), Some(&last)) if old_len == self.messages.len() && first == last => {
                TranscriptChange::Updated { row: first }
            }
            _ => TranscriptChange::Reset,
        }
    }

    fn touch(&mut self, row: usize) {
        self.revision += 1;
        self.changed_at
            .insert(self.messages[row].info.id.clone(), self.revision);
    }

    fn reindex(&mut self, from: usize) {
        if from == 0 {
            self.row_by_id.clear();
        }
        for (row, message) in self.messages.iter().enumerate().skip(from) {
            self.row_by_id.insert(message.info.id.clone(), row);
        }
    }
}

fn is_final_tool(part: &Part) -> bool {
    matches!(
        part.content,
        PartContent::Tool {
            status: ToolStatus::Completed | ToolStatus::Error,
            ..
        }
    )
}

fn is_active_tool(part: &Part) -> bool {
    matches!(
        part.content,
        PartContent::Tool {
            status: ToolStatus::Pending | ToolStatus::Running,
            ..
        }
    )
}

fn part_text(part: &Part) -> Option<&str> {
    match &part.content {
        PartContent::Text { text } | PartContent::Reasoning { text } => Some(text),
        _ => None,
    }
}

/// Whether a full snapshot repeats or extends the streamed text, so the next
/// delta may overlap its end (`getUpdatedDeltaFields` in `event-reducer.ts`).
fn may_repeat_text(previous: &Part, next: &Part) -> bool {
    if std::mem::discriminant(&previous.content) != std::mem::discriminant(&next.content) {
        return false;
    }
    match (part_text(previous), part_text(next)) {
        (Some(previous), Some(next)) if !previous.is_empty() && !next.is_empty() => {
            next.starts_with(previous) || previous.starts_with(next)
        }
        _ => false,
    }
}

/// Appends the part of `delta` that `text` does not already end with, as
/// `appendNonOverlappingDelta` does. Returns whether anything was appended.
fn append_without_overlap(text: &mut String, delta: &str) -> bool {
    if text.is_empty() {
        text.push_str(delta);
        return true;
    }
    if text.ends_with(delta) {
        return false;
    }
    let mut boundaries: Vec<usize> = delta
        .char_indices()
        .map(|(index, _)| index)
        .skip(1)
        .chain([delta.len()])
        .filter(|end| *end <= text.len())
        .collect();
    boundaries.reverse();
    for end in boundaries {
        if text.ends_with(&delta[..end]) {
            text.push_str(&delta[end..]);
            return true;
        }
    }
    text.push_str(delta);
    true
}

/// A message from a history read, merged with the copy this client holds.
///
/// The held record stands unless the read reports a completion it lacks.
/// Assistant parts merge one by one; parts only the client holds that are
/// still streaming stay.
fn merge_message(held: &Message, incoming: Message) -> Message {
    let is_assistant = incoming.info.role == Role::Assistant;
    let info = if held.info.role == Role::Assistant
        && incoming.info.role == Role::Assistant
        && incoming.info.completed.is_some()
        && held.info.completed.is_none()
    {
        incoming.info
    } else {
        held.info.clone()
    };
    if is_assistant {
        let mut parts: Vec<Part> = incoming
            .parts
            .into_iter()
            .map(
                |next| match held.parts.iter().find(|part| part.id == next.id) {
                    Some(existing) => merge_part(existing, next),
                    None => next,
                },
            )
            .collect();
        for existing in &held.parts {
            let streaming = part_text(existing).is_some_and(|text| !text.is_empty());
            if streaming && !parts.iter().any(|part| part.id == existing.id) {
                parts.push(existing.clone());
            }
        }
        Message { info, parts }
    } else {
        Message {
            info,
            parts: incoming.parts,
        }
    }
}

/// One part from a history read, merged with the held copy
/// (`mergeMaterializedPart` in `materialization.ts`).
fn merge_part(existing: &Part, next: Part) -> Part {
    if let (
        PartContent::Tool {
            status: held_status,
            elided: held_elided,
            ..
        },
        PartContent::Tool {
            status: next_status,
            elided: next_elided,
            ..
        },
    ) = (&existing.content, &next.content)
    {
        let shell_of_whole =
            held_status == next_status && next_elided.is_some() && held_elided.is_none();
        let regressed = is_final_tool(existing) && is_active_tool(&next);
        let earlier = *held_status == ToolStatus::Running && *next_status == ToolStatus::Pending;
        return if shell_of_whole || regressed || earlier {
            existing.clone()
        } else {
            next
        };
    }
    match (part_text(existing), part_text(&next)) {
        // The client streamed further than the read saw.
        (Some(held), Some(read))
            if std::mem::discriminant(&existing.content)
                == std::mem::discriminant(&next.content)
                && held.len() > read.len()
                && held.starts_with(read) =>
        {
            existing.clone()
        }
        _ => next,
    }
}

/// Puts back whole tool results that `next` carries only as shells.
///
/// Mirrors `mergeMaterializedPart` in `packages/ui/src/sync/materialization.ts`:
/// a shell replaces nothing the client holds whole for the same call status.
fn keep_whole_results(held: &Message, next: &mut Message) {
    for part in &mut next.parts {
        let PartContent::Tool {
            status,
            elided: Some(_),
            ..
        } = &part.content
        else {
            continue;
        };
        let whole = held.parts.iter().find(|candidate| {
            candidate.id == part.id
                && matches!(
                    &candidate.content,
                    PartContent::Tool { status: held_status, elided: None, .. }
                        if held_status == status
                )
        });
        if let Some(whole) = whole {
            part.content = whole.content.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, role: Role, text: &str) -> Message {
        Message {
            info: MessageInfo {
                id: MessageId::from(id),
                role,
                created: 0,
                completed: None,
                notice: None,
                assistant: None,
            },
            parts: vec![Part::text(format!("{id}-p0").as_str(), text)],
        }
    }

    #[test]
    fn turns_run_from_a_prompt_to_the_next_and_skip_notices() {
        let created = |mut message: Message, at: i64| {
            message.info.created = at;
            message
        };
        let transcript = Transcript::from_history(vec![
            created(message("u1", Role::User, "go"), 100),
            message("a1", Role::Assistant, "step"),
            message("a2", Role::Assistant, "answer"),
            message("i1", Role::Idle, ""),
            created(message("u2", Role::User, "again"), 200),
            message("a3", Role::Assistant, "answer"),
            message("s1", Role::ModelSwitched, ""),
        ]);
        let position = |row| transcript.turn_position(row);
        assert_eq!(
            position(1),
            Some(TurnPosition {
                last_assistant: false,
                prompt_created: Some(100)
            })
        );
        assert_eq!(
            position(2),
            Some(TurnPosition {
                last_assistant: true,
                prompt_created: Some(100)
            }),
            "an idle record does not continue the turn"
        );
        assert_eq!(
            position(5),
            Some(TurnPosition {
                last_assistant: true,
                prompt_created: Some(200)
            })
        );
        assert_eq!(position(0), None, "user rows have no turn position");
        assert_eq!(position(3), None);
    }

    #[test]
    fn deltas_update_one_row_and_unchanged_events_do_not_notify() {
        let mut transcript = Transcript::from_history(vec![
            message("m1", Role::User, "hi"),
            message("m2", Role::Assistant, "he"),
        ]);
        let revision = transcript.revision();
        let change = transcript.apply(TranscriptEvent::PartDelta {
            message_id: MessageId::from("m2"),
            part_id: PartId::from("m2-p0"),
            delta: "llo".to_owned(),
        });
        assert_eq!(change, TranscriptChange::Updated { row: 1 });
        assert_eq!(
            transcript.message(1).map(Message::plain_text).as_deref(),
            Some("hello")
        );
        assert_eq!(transcript.revision(), revision + 1);

        let same = transcript.message(1).expect("row exists").parts[0].clone();
        let change = transcript.apply(TranscriptEvent::PartUpdated {
            message_id: MessageId::from("m2"),
            part: same,
        });
        assert_eq!(change, TranscriptChange::Unchanged);
        assert_eq!(transcript.revision(), revision + 1);
    }

    #[test]
    fn server_order_holds_regardless_of_id_spelling() {
        let mut transcript = Transcript::from_history(vec![message("z", Role::User, "first")]);
        let change = transcript.push(message("a", Role::Assistant, "second"));
        assert_eq!(change, TranscriptChange::Inserted { rows: 1..2 });
        let order: Vec<&str> = transcript
            .messages()
            .iter()
            .map(|message| message.info.id.as_str())
            .collect();
        assert_eq!(order, ["z", "a"]);
    }

    #[test]
    fn prepend_skips_held_messages_and_reindexes() {
        let mut transcript = Transcript::from_history(vec![message("m3", Role::User, "c")]);
        let change = transcript.prepend(vec![
            message("m1", Role::User, "a"),
            message("m2", Role::Assistant, "b"),
            message("m3", Role::User, "c"),
        ]);
        assert_eq!(change, TranscriptChange::Inserted { rows: 0..2 });
        assert_eq!(transcript.row_of(&MessageId::from("m3")), Some(2));
    }

    #[test]
    fn events_for_unknown_messages_request_reconciliation() {
        let mut transcript = Transcript::new();
        let change = transcript.apply(TranscriptEvent::PartDelta {
            message_id: MessageId::from("missing"),
            part_id: PartId::from("p"),
            delta: "x".to_owned(),
        });
        assert_eq!(change, TranscriptChange::NeedsReconcile);
    }

    #[test]
    fn tool_transitions_keep_the_tool_name() {
        let mut transcript = Transcript::from_history(vec![Message {
            info: MessageInfo {
                id: MessageId::from("m1"),
                role: Role::Assistant,
                created: 0,
                completed: None,
                notice: None,
                assistant: None,
            },
            parts: vec![Part {
                id: PartId::from("call_1"),
                content: PartContent::Tool {
                    tool: "read".to_owned(),
                    status: ToolStatus::Pending,
                    title: None,
                    output: None,
                    elided: None,
                },
            }],
        }]);
        let change = transcript.apply(TranscriptEvent::ToolTransition {
            message_id: MessageId::from("m1"),
            part_id: PartId::from("call_1"),
            status: ToolStatus::Completed,
            output: Some("ok".to_owned()),
        });
        assert_eq!(change, TranscriptChange::Updated { row: 0 });
        let part = &transcript.message(0).expect("row exists").parts[0];
        assert!(matches!(
            &part.content,
            PartContent::Tool { tool, status: ToolStatus::Completed, output: Some(output), .. }
                if tool == "read" && output == "ok"
        ));
        let completed = transcript.apply(TranscriptEvent::MessageCompleted {
            message_id: MessageId::from("m1"),
            completed: 9,
            outcome: StepOutcome::default(),
        });
        assert_eq!(completed, TranscriptChange::Updated { row: 0 });
        assert_eq!(
            transcript
                .message(0)
                .and_then(|message| message.info.completed),
            Some(9)
        );
    }

    #[test]
    fn a_shelled_page_keeps_whole_results() {
        let tool = |output: &str, elided: Option<usize>| Message {
            info: MessageInfo {
                id: MessageId::from("m1"),
                role: Role::Assistant,
                created: 0,
                completed: Some(1),
                notice: None,
                assistant: None,
            },
            parts: vec![Part {
                id: PartId::from("call_1"),
                content: PartContent::Tool {
                    tool: "glob".to_owned(),
                    status: ToolStatus::Completed,
                    title: None,
                    output: Some(output.to_owned()),
                    elided,
                },
            }],
        };
        let mut transcript = Transcript::from_history(vec![tool("a.rs", None)]);
        assert_eq!(
            transcript.replace(vec![tool("", Some(4))]),
            TranscriptChange::Unchanged,
            "the shell names the result this transcript already holds"
        );
        assert_eq!(
            transcript.push(tool("", Some(4))),
            TranscriptChange::Unchanged
        );
        let mut shelled = Transcript::from_history(vec![tool("", Some(4))]);
        assert_eq!(
            shelled.push(tool("a.rs", None)),
            TranscriptChange::Updated { row: 0 },
            "a whole message completes a shelled call"
        );
    }

    fn text_message(id: &str, role: Role, text: &str) -> Message {
        Message {
            info: MessageInfo {
                id: MessageId::from(id),
                role,
                created: 0,
                completed: None,
                notice: None,
                assistant: None,
            },
            parts: vec![Part::text(format!("{id}:text:0"), text)],
        }
    }

    fn ids(transcript: &Transcript) -> Vec<&str> {
        transcript
            .messages()
            .iter()
            .map(|message| message.info.id.as_str())
            .collect()
    }

    #[test]
    fn materialize_keeps_older_pages_and_live_arrivals_and_drops_deletions() {
        let mut transcript = Transcript::from_history(vec![
            text_message("old", Role::User, "older page"),
            text_message("a", Role::User, "a"),
            text_message("gone", Role::User, "deleted on the server"),
            text_message("b", Role::Assistant, "b"),
        ]);
        let since = transcript.revision();
        let _ = transcript.push(text_message("live", Role::User, "sent after the read"));
        let change = transcript.materialize(
            vec![
                text_message("a", Role::User, "a"),
                text_message("b", Role::Assistant, "b"),
                text_message("c", Role::Assistant, "c"),
            ],
            since,
        );
        assert_eq!(change, TranscriptChange::Reset);
        assert_eq!(ids(&transcript), ["old", "a", "b", "c", "live"]);
        assert_eq!(transcript.row_of(&MessageId::from("live")), Some(4));
    }

    #[test]
    fn materialize_appends_without_resetting() {
        let mut transcript = Transcript::from_history(vec![text_message("a", Role::User, "a")]);
        let since = transcript.revision();
        let change = transcript.materialize(
            vec![
                text_message("a", Role::User, "a"),
                text_message("b", Role::Assistant, "b"),
            ],
            since,
        );
        assert_eq!(change, TranscriptChange::Inserted { rows: 1..2 });
        assert_eq!(
            transcript.materialize(transcript.messages().to_vec(), since),
            TranscriptChange::Unchanged
        );
    }

    #[test]
    fn materialize_keeps_streamed_text_ahead_of_the_read_and_takes_completion() {
        let mut transcript =
            Transcript::from_history(vec![text_message("m", Role::Assistant, "hello wor")]);
        let since = transcript.revision();
        let mut read = text_message("m", Role::Assistant, "hello");
        read.info.completed = Some(5);
        let change = transcript.materialize(vec![read], since);
        assert_eq!(change, TranscriptChange::Updated { row: 0 });
        let message = transcript.message(0).expect("row exists");
        assert_eq!(message.plain_text(), "hello wor");
        assert_eq!(message.info.completed, Some(5));
    }

    #[test]
    fn deltas_need_a_started_part_and_skip_text_a_snapshot_repeated() {
        let mut transcript =
            Transcript::from_history(vec![text_message("m", Role::Assistant, "ab")]);
        let orphan = transcript.apply(TranscriptEvent::PartDelta {
            message_id: MessageId::from("m"),
            part_id: PartId::from("m:text:1"),
            delta: "x".to_owned(),
        });
        assert_eq!(orphan, TranscriptChange::NeedsReconcile);
        let _ = transcript.apply(TranscriptEvent::PartUpdated {
            message_id: MessageId::from("m"),
            part: Part::text("m:text:0", "abcd"),
        });
        let _ = transcript.apply(TranscriptEvent::PartDelta {
            message_id: MessageId::from("m"),
            part_id: PartId::from("m:text:0"),
            delta: "cdef".to_owned(),
        });
        let _ = transcript.apply(TranscriptEvent::PartDelta {
            message_id: MessageId::from("m"),
            part_id: PartId::from("m:text:0"),
            delta: "fg".to_owned(),
        });
        assert_eq!(
            transcript.message(0).expect("row exists").plain_text(),
            "abcdeffg",
            "only the first delta after a snapshot drops its repeated start"
        );
    }

    #[test]
    fn a_finished_call_never_returns_to_an_active_state() {
        let tool = |status: ToolStatus| Part {
            id: PartId::from("call_1"),
            content: PartContent::Tool {
                tool: "glob".to_owned(),
                status,
                title: None,
                output: None,
                elided: None,
            },
        };
        let mut transcript = Transcript::from_history(vec![Message {
            info: MessageInfo {
                id: MessageId::from("m"),
                role: Role::Assistant,
                created: 0,
                completed: None,
                notice: None,
                assistant: None,
            },
            parts: vec![tool(ToolStatus::Completed)],
        }]);
        let change = transcript.apply(TranscriptEvent::PartUpdated {
            message_id: MessageId::from("m"),
            part: tool(ToolStatus::Running),
        });
        assert_eq!(change, TranscriptChange::Unchanged);
    }

    #[test]
    fn removal_reindexes_later_rows() {
        let mut transcript = Transcript::from_history(vec![
            message("m1", Role::User, "a"),
            message("m2", Role::Assistant, "b"),
            message("m3", Role::User, "c"),
        ]);
        let change = transcript.apply(TranscriptEvent::MessageRemoved {
            message_id: MessageId::from("m1"),
        });
        assert_eq!(change, TranscriptChange::Removed { rows: 0..1 });
        assert_eq!(transcript.row_of(&MessageId::from("m3")), Some(1));
        assert_eq!(transcript.row_of(&MessageId::from("m1")), None);
    }
}
