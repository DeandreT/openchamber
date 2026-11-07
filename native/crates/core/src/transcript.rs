//! Ordered session transcripts with stable message and part identities.
//!
//! Server order is authoritative: history arrives in order, live messages
//! append, older pages prepend. Identifier spelling never reorders anything.
//! Every mutation reports which rows changed so the renderer can update one
//! row for a text delta without rebuilding the list.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::ids::{MessageId, PartId, SessionId};
use crate::send_config::ModelChoice;
use crate::tool_shells::ToolFacts;

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
        /// The child session a `subagent` call runs in, once the call names it.
        child: Option<SessionId>,
        /// The call's input and metadata as the row reads them.
        facts: Box<ToolFacts>,
    },
    File {
        filename: String,
        mime: String,
        /// Where the file loads from: a URI or a `data:` URL.
        url: Option<String>,
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
    pub time: PartTime,
}

/// When a part was produced, as far as its record says: a history read
/// gives text its step's window and a tool its run, the live stream the
/// times its events arrived.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PartTime {
    pub start: Option<i64>,
    pub end: Option<i64>,
}

impl Part {
    pub fn text(id: impl Into<PartId>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: PartContent::Text { text: text.into() },
            time: PartTime::default(),
        }
    }
}

/// A step's cost in billionths of a US dollar, so costs add exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Usd(u64);

impl Usd {
    /// `None` for a negative or non-finite amount.
    pub fn from_dollars(dollars: f64) -> Option<Self> {
        (dollars.is_finite() && dollars >= 0.0).then(|| Self((dollars * 1e9).round() as u64))
    }

    pub fn dollars(self) -> f64 {
        self.0 as f64 / 1e9
    }
}

impl std::ops::Add for Usd {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
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
    pub cost: Option<Usd>,
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
    /// A compaction record's lifecycle; `None` for every other role.
    pub compaction: Option<CompactionStatus>,
    /// What a synthetic record's description and metadata establish; `None`
    /// for every other role.
    pub synthetic: Option<Box<SyntheticFacts>>,
}

/// A synthetic record's description and the metadata the client reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyntheticFacts {
    pub description: Option<String>,
    /// `source: "subagent"`: the report of a background subagent run.
    pub subagent: Option<SubagentRun>,
    /// `source: "openchamber-session"`: the result of a session an agent
    /// dispatched (`readDispatchedSessionResult`), naming that session.
    pub dispatched_session: Option<SessionId>,
    /// `source: "openchamber-workflow"`: a workflow run's report
    /// (`readWorkflowReport`), naming the run.
    pub workflow_run: Option<String>,
}

/// A background subagent run as its report names it (`readSubagentRun`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentRun {
    /// `metadata.childID`.
    pub child: SessionId,
    pub agent: Option<String>,
    pub state: SubagentRunState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SubagentRunState {
    Running,
    Completed,
    Error,
    Cancelled,
}

/// Where a compaction stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CompactionStatus {
    Running,
    Completed,
    Failed,
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
        /// What the transition reports about the call's input and metadata;
        /// what it leaves out stays.
        facts: Option<Box<ToolFacts>>,
        /// When the transition happened: a call's start, a result's end.
        at: i64,
    },
    /// A `subagent` call named the child session it runs in.
    ToolChild {
        message_id: MessageId,
        part_id: PartId,
        child: SessionId,
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
    /// Deletions stay known while this transcript lives, including messages
    /// not loaded yet, so older in-flight pages cannot restore them.
    removed_at: HashMap<MessageId, u64>,
    /// Text parts whose next delta may repeat the end of a full snapshot.
    overlapping_delta: HashSet<PartId>,
    /// The newest committed revert's boundary and the revision it arrived
    /// at; reads requested before it never bring the boundary or anything
    /// after it back.
    committed: Option<(MessageId, u64)>,
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

    /// Prepends a page requested at `since`, preserving newer deletions.
    pub fn prepend_page(&mut self, page: Vec<Message>, since: u64) -> TranscriptChange {
        let Some(page) = self.without_committed(page, since) else {
            return TranscriptChange::NeedsReconcile;
        };
        let page = page
            .into_iter()
            .filter(|message| {
                !self
                    .removed_at
                    .get(&message.info.id)
                    .is_some_and(|revision| *revision > since)
            })
            .collect();
        self.prepend(page)
    }

    /// Applies one authoritative message read at `since`, preserving newer
    /// changes or deletions of that message and leaving all other rows intact.
    pub fn apply_message_snapshot(&mut self, message: Message, since: u64) -> TranscriptChange {
        let id = &message.info.id;
        if self
            .changed_at
            .get(id)
            .is_some_and(|revision| *revision > since)
            || self
                .removed_at
                .get(id)
                .is_some_and(|revision| *revision > since)
        {
            return TranscriptChange::Unchanged;
        }
        self.push(message)
    }

    /// The answer a turn's footer copies: the text parts of every assistant
    /// message from the turn's prompt through `row`, in order, without tool
    /// output or reasoning.
    pub fn turn_answer_text(&self, row: usize) -> String {
        let Some(end) = self.messages.get(..=row) else {
            return String::new();
        };
        let start = end
            .iter()
            .rposition(|message| message.info.role == Role::User)
            .map_or(0, |prompt| prompt + 1);
        let texts: Vec<&str> = end[start..]
            .iter()
            .filter(|message| message.info.role == Role::Assistant)
            .flat_map(|message| &message.parts)
            .filter_map(|part| match &part.content {
                PartContent::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
                _ => None,
            })
            .collect();
        texts.join("\n\n")
    }

    /// Fills in the tool results a history page left out from a whole read
    /// of the same message. Only calls still held as shells change, and only
    /// with a whole result of the same status, so newer live changes to the
    /// message survive a read that started before them.
    pub fn complete_elided(&mut self, whole: &Message) -> TranscriptChange {
        let Some(row) = self.row_of(&whole.info.id) else {
            return TranscriptChange::Unchanged;
        };
        let mut changed = false;
        for part in &mut self.messages[row].parts {
            let PartContent::Tool {
                status,
                elided: Some(_),
                ..
            } = &part.content
            else {
                continue;
            };
            let complete = whole.parts.iter().find(|candidate| {
                candidate.id == part.id
                    && matches!(
                        &candidate.content,
                        PartContent::Tool { status: read, elided: None, .. } if read == status
                    )
            });
            if let Some(complete) = complete {
                part.content = complete.content.clone();
                changed = true;
            }
        }
        if !changed {
            return TranscriptChange::Unchanged;
        }
        self.touch(row);
        TranscriptChange::Updated { row }
    }

    /// Appends a message, or updates it in place when already held.
    pub fn push(&mut self, mut message: Message) -> TranscriptChange {
        self.removed_at.remove(&message.info.id);
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
                        *existing = with_streamed_start(existing, part);
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
                self.revision += 1;
                self.removed_at.insert(message_id.clone(), self.revision);
                let Some(row) = self.row_of(&message_id) else {
                    return TranscriptChange::Unchanged;
                };
                self.messages.remove(row);
                self.row_by_id.remove(&message_id);
                self.changed_at.remove(&message_id);
                self.reindex(row);
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
                facts: next_facts,
                at,
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
                            facts,
                            ..
                        },
                    time,
                    ..
                }) = part
                else {
                    return TranscriptChange::NeedsReconcile;
                };
                // A transition with output carries the whole result.
                let next_elided = if next_output.is_some() { None } else { *elided };
                let next_output = next_output.or_else(|| output.clone());
                // `applyToolTransition`: a call starts the run; a result ends
                // it and keeps the start a running call recorded.
                let next_time = match next_status {
                    ToolStatus::Running => PartTime {
                        start: Some(at),
                        end: None,
                    },
                    ToolStatus::Completed | ToolStatus::Error => PartTime {
                        start: time
                            .start
                            .filter(|_| *status == ToolStatus::Running)
                            .or(Some(at)),
                        end: Some(at),
                    },
                    ToolStatus::Pending => *time,
                };
                let merged = next_facts.map(|next| {
                    let mut merged = facts.as_ref().clone();
                    merged.merge(*next);
                    merged
                });
                if *status == next_status
                    && *output == next_output
                    && *elided == next_elided
                    && *time == next_time
                    && merged
                        .as_ref()
                        .is_none_or(|merged| merged == facts.as_ref())
                {
                    return TranscriptChange::Unchanged;
                }
                if let Some(merged) = merged {
                    **facts = merged;
                }
                *status = next_status;
                *output = next_output;
                *elided = next_elided;
                *time = next_time;
                self.touch(row);
                TranscriptChange::Updated { row }
            }
            TranscriptEvent::ToolChild {
                message_id,
                part_id,
                child: next,
            } => {
                let Some(row) = self.row_of(&message_id) else {
                    return TranscriptChange::NeedsReconcile;
                };
                let Some(Part {
                    content: PartContent::Tool { child, .. },
                    ..
                }) = self.messages[row]
                    .parts
                    .iter_mut()
                    .find(|part| part.id == part_id)
                else {
                    return TranscriptChange::NeedsReconcile;
                };
                if child.as_ref() == Some(&next) {
                    return TranscriptChange::Unchanged;
                }
                *child = Some(next);
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

    /// A committed revert: OpenCode deleted `to` and everything after it
    /// without per-message removals (`session.revert.committed` in
    /// `event-reducer.ts`). The held window is always the transcript's tail,
    /// so a boundary it does not hold but the session's marker names is older
    /// than everything loaded and trims it all. Every trimmed ID is recorded
    /// as removed, so an older read cannot bring it back.
    pub fn truncate_from(
        &mut self,
        to: &MessageId,
        marker: Option<&MessageId>,
    ) -> TranscriptChange {
        // Remembered even when nothing is loaded yet, so a read still in
        // flight cannot show what the commit deleted.
        self.revision += 1;
        self.committed = Some((to.clone(), self.revision));
        let from = match self.row_of(to) {
            Some(row) => row,
            None if marker == Some(to) => 0,
            None => return TranscriptChange::Unchanged,
        };
        if from >= self.messages.len() {
            return TranscriptChange::Unchanged;
        }
        let removed: Vec<Message> = self.messages.drain(from..).collect();
        for message in &removed {
            self.row_by_id.remove(&message.info.id);
            self.changed_at.remove(&message.info.id);
            self.removed_at
                .insert(message.info.id.clone(), self.revision);
        }
        TranscriptChange::Removed {
            rows: from..from + removed.len(),
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
        let Some(history) = self.without_committed(history, since) else {
            return TranscriptChange::NeedsReconcile;
        };
        let history: Vec<Message> = history
            .into_iter()
            .filter(|message| {
                !self
                    .removed_at
                    .get(&message.info.id)
                    .is_some_and(|revision| *revision > since)
            })
            .collect();
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

    /// A read in server order requested before the newest commit loses the
    /// committed boundary and everything after it. Without the boundary,
    /// the caller must reconcile; the page cannot prove what survived.
    fn without_committed(&self, mut read: Vec<Message>, since: u64) -> Option<Vec<Message>> {
        if let Some((to, at)) = &self.committed
            && *at > since
        {
            // Without the boundary this page could be entirely before or
            // after the cut. It has no replacement or insertion authority.
            let index = read.iter().position(|message| &message.info.id == to)?;
            read.truncate(index);
        }
        Some(read)
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

/// A text or reasoning snapshot that ends a part carries its own time as the
/// start; the start streamed earlier is the real one and stays
/// (`withStreamedStart` in `event-reducer.ts`).
fn with_streamed_start(previous: &Part, mut next: Part) -> Part {
    let same_kind = matches!(
        (&previous.content, &next.content),
        (PartContent::Text { .. }, PartContent::Text { .. })
            | (PartContent::Reasoning { .. }, PartContent::Reasoning { .. })
    );
    if let (true, Some(_), Some(earlier), Some(start)) = (
        same_kind,
        next.time.end,
        previous.time.start,
        next.time.start,
    ) && earlier < start
    {
        next.time.start = Some(earlier);
    }
    next
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
        if shell_of_whole || regressed || earlier {
            return existing.clone();
        }
        // A running call's child comes only with live progress; a read of
        // the running call does not carry it.
        let mut next = next;
        if let (
            PartContent::Tool {
                child: Some(held_child),
                ..
            },
            PartContent::Tool {
                child: child @ None,
                ..
            },
        ) = (&existing.content, &mut next.content)
        {
            *child = Some(held_child.clone());
        }
        return next;
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

    #[test]
    fn late_exact_snapshot_preserves_target_deletion() {
        for loaded in [false, true] {
            let stale = message("target", Role::User, "old");
            let mut transcript = Transcript::new();
            if loaded {
                let _ = transcript.push(stale.clone());
            }
            let since = transcript.revision();
            let _ = transcript.apply(TranscriptEvent::MessageRemoved {
                message_id: stale.info.id.clone(),
            });
            let before = transcript.clone();
            assert_eq!(
                transcript.apply_message_snapshot(stale, since),
                TranscriptChange::Unchanged
            );
            assert_eq!(
                transcript, before,
                "late lookup preserves the deletion tombstone"
            );
        }
    }

    #[test]
    fn late_exact_snapshot_preserves_target_change() {
        let stale = message("target", Role::User, "old");
        let mut transcript = Transcript::from_history(vec![stale.clone()]);
        let since = transcript.revision();
        let _ = transcript.apply(TranscriptEvent::PartsReplaced {
            message_id: stale.info.id.clone(),
            parts: vec![Part::text("target-p0", "newer live content")],
        });
        let before = transcript.clone();
        assert_eq!(
            transcript.apply_message_snapshot(stale, since),
            TranscriptChange::Unchanged
        );
        assert_eq!(
            transcript, before,
            "late lookup preserves newer target content"
        );
    }

    #[test]
    fn exact_snapshot_accepts_unchanged_target_after_unrelated_change() {
        let mut transcript = Transcript::from_history(vec![
            message("target", Role::User, "optimistic"),
            message("other", Role::Assistant, "old"),
        ]);
        let since = transcript.revision();
        let other = message("other", Role::Assistant, "newer live content");
        let _ = transcript.push(other.clone());
        let authoritative = message("target", Role::User, "prepared by server");
        assert_eq!(
            transcript.apply_message_snapshot(authoritative.clone(), since),
            TranscriptChange::Updated { row: 0 }
        );
        assert_eq!(transcript.messages(), &[authoritative, other]);
    }

    #[test]
    fn delayed_history_cannot_restore_a_deleted_message() {
        let deleted = message("deleted", Role::User, "old");
        let kept = message("kept", Role::Assistant, "kept");
        let mut transcript = Transcript::from_history(vec![deleted.clone(), kept.clone()]);
        let since = transcript.revision();
        let _ = transcript.apply(TranscriptEvent::MessageRemoved {
            message_id: deleted.info.id.clone(),
        });
        assert_eq!(
            transcript.materialize(vec![deleted.clone(), kept], since),
            TranscriptChange::Unchanged
        );
        assert!(
            transcript.row_of(&deleted.info.id).is_none(),
            "a stale newest-page read cannot undo a deletion"
        );
        assert_eq!(
            transcript.prepend_page(vec![deleted.clone()], since),
            TranscriptChange::Unchanged
        );
        assert!(
            transcript.row_of(&deleted.info.id).is_none(),
            "a stale older page cannot undo a deletion"
        );

        let unseen = message("unseen", Role::User, "older");
        let since = transcript.revision();
        let _ = transcript.apply(TranscriptEvent::MessageRemoved {
            message_id: unseen.info.id.clone(),
        });
        assert_eq!(
            transcript.prepend_page(vec![unseen.clone()], since),
            TranscriptChange::Unchanged
        );
        assert!(
            transcript.row_of(&unseen.info.id).is_none(),
            "deletions also cover messages not loaded yet"
        );
        let _ = transcript.push(unseen.clone());
        assert!(
            transcript.row_of(&unseen.info.id).is_some(),
            "a new authoritative live record may restore an id"
        );
    }

    fn message(id: &str, role: Role, text: &str) -> Message {
        Message {
            info: MessageInfo {
                id: MessageId::from(id),
                role,
                created: 0,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
                synthetic: None,
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
                compaction: None,
                synthetic: None,
            },
            parts: vec![Part {
                id: PartId::from("call_1"),
                content: PartContent::Tool {
                    tool: "read".to_owned(),
                    status: ToolStatus::Pending,
                    title: None,
                    output: None,
                    elided: None,
                    child: None,
                    facts: Box::default(),
                },
                time: PartTime::default(),
            }],
        }]);
        let change = transcript.apply(TranscriptEvent::ToolTransition {
            message_id: MessageId::from("m1"),
            part_id: PartId::from("call_1"),
            status: ToolStatus::Completed,
            output: Some("ok".to_owned()),
            at: 8,
            facts: None,
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
    fn a_turn_answer_joins_its_assistant_text() {
        let message = |id: &str, role: Role, parts: Vec<Part>| Message {
            info: MessageInfo {
                id: MessageId::from(id),
                role,
                created: 0,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
                synthetic: None,
            },
            parts,
        };
        let reasoning = Part {
            id: PartId::from("r"),
            content: PartContent::Reasoning {
                text: "thinking".to_owned(),
            },
            time: PartTime::default(),
        };
        let transcript = Transcript::from_history(vec![
            message("u1", Role::User, vec![Part::text("u1t", "first")]),
            message("a1", Role::Assistant, vec![Part::text("a1t", "old answer")]),
            message("u2", Role::User, vec![Part::text("u2t", "second")]),
            message(
                "a2",
                Role::Assistant,
                vec![reasoning, Part::text("a2t", "Step one.")],
            ),
            message("a3", Role::Assistant, vec![Part::text("a3t", "Done.")]),
        ]);
        assert_eq!(transcript.turn_answer_text(4), "Step one.\n\nDone.");
        assert_eq!(transcript.turn_answer_text(1), "old answer");
        assert_eq!(transcript.turn_answer_text(9), "");
    }

    #[test]
    fn a_whole_read_completes_only_calls_still_shelled() {
        let message = |parts: Vec<Part>| Message {
            info: MessageInfo {
                id: MessageId::from("m1"),
                role: Role::Assistant,
                created: 0,
                completed: Some(1),
                notice: None,
                assistant: None,
                compaction: None,
                synthetic: None,
            },
            parts,
        };
        let call = |id: &str, status: ToolStatus, output: &str, elided: Option<usize>| Part {
            id: PartId::from(id),
            content: PartContent::Tool {
                tool: "shell".to_owned(),
                status,
                title: None,
                output: Some(output.to_owned()),
                elided,
                child: None,
                facts: Box::default(),
            },
            time: PartTime::default(),
        };
        let mut transcript = Transcript::from_history(vec![message(vec![
            call("a", ToolStatus::Completed, "", Some(3)),
            call("b", ToolStatus::Error, "", Some(5)),
            Part::text("t", "newer live text"),
        ])]);
        let whole = message(vec![
            call("a", ToolStatus::Completed, "one", None),
            call("b", ToolStatus::Completed, "two", None),
            Part::text("t", "older"),
        ]);
        assert_eq!(
            transcript.complete_elided(&whole),
            TranscriptChange::Updated { row: 0 }
        );
        let parts = &transcript.message(0).expect("row exists").parts;
        assert!(matches!(
            &parts[0].content,
            PartContent::Tool { output: Some(output), elided: None, .. } if output == "one"
        ));
        assert!(
            matches!(
                &parts[1].content,
                PartContent::Tool {
                    elided: Some(5),
                    ..
                }
            ),
            "a read of another status leaves the shell"
        );
        assert_eq!(
            parts[2].content,
            PartContent::Text {
                text: "newer live text".to_owned()
            }
        );
        assert_eq!(
            transcript.complete_elided(&whole),
            TranscriptChange::Unchanged
        );
    }

    #[test]
    fn a_transition_merges_the_facts_it_reports() {
        use crate::tool_shells::{ToolFacts, ToolInput};

        let mut transcript = Transcript::from_history(vec![Message {
            info: MessageInfo {
                id: MessageId::from("m1"),
                role: Role::Assistant,
                created: 0,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
                synthetic: None,
            },
            parts: vec![Part {
                id: PartId::from("call_1"),
                content: PartContent::Tool {
                    tool: "shell".to_owned(),
                    status: ToolStatus::Pending,
                    title: None,
                    output: None,
                    elided: None,
                    child: None,
                    facts: Box::default(),
                },
                time: PartTime::default(),
            }],
        }]);
        let transition = |status, facts: Option<ToolFacts>| TranscriptEvent::ToolTransition {
            message_id: MessageId::from("m1"),
            part_id: PartId::from("call_1"),
            status,
            output: None,
            facts: facts.map(Box::new),
            at: 5,
        };
        let called = ToolFacts {
            input: ToolInput {
                command: Some("ls".to_owned()),
                ..ToolInput::default()
            },
            ..ToolFacts::default()
        };
        assert_eq!(
            transcript.apply(transition(ToolStatus::Running, Some(called))),
            TranscriptChange::Updated { row: 0 }
        );
        let failed = ToolFacts {
            error: Some("exit 2".to_owned()),
            ..ToolFacts::default()
        };
        let _ = transcript.apply(transition(ToolStatus::Error, Some(failed)));
        let PartContent::Tool { facts, .. } = &transcript.message(0).expect("row").parts[0].content
        else {
            panic!("the call stays a tool part");
        };
        assert_eq!(facts.input.command.as_deref(), Some("ls"));
        assert_eq!(facts.error.as_deref(), Some("exit 2"));
        assert_eq!(
            transcript.apply(transition(ToolStatus::Error, Some(ToolFacts::default()))),
            TranscriptChange::Unchanged,
            "facts that add nothing change nothing"
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
                compaction: None,
                synthetic: None,
            },
            parts: vec![Part {
                id: PartId::from("call_1"),
                content: PartContent::Tool {
                    tool: "glob".to_owned(),
                    status: ToolStatus::Completed,
                    title: None,
                    output: Some(output.to_owned()),
                    elided,
                    child: None,
                    facts: Box::default(),
                },
                time: PartTime::default(),
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
                compaction: None,
                synthetic: None,
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
                child: None,
                facts: Box::default(),
            },
            time: PartTime::default(),
        };
        let mut transcript = Transcript::from_history(vec![Message {
            info: MessageInfo {
                id: MessageId::from("m"),
                role: Role::Assistant,
                created: 0,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
                synthetic: None,
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

    #[test]
    fn a_committed_revert_trims_the_tail_for_good() {
        let history = vec![
            message("m1", Role::User, "a"),
            message("m2", Role::Assistant, "b"),
            message("m3", Role::User, "c"),
            message("m4", Role::Assistant, "d"),
        ];
        let mut transcript = Transcript::from_history(history.clone());
        let since = transcript.revision();
        assert_eq!(
            transcript.truncate_from(&MessageId::from("m3"), None),
            TranscriptChange::Removed { rows: 2..4 }
        );
        assert_eq!(transcript.len(), 2);
        assert_eq!(
            transcript.truncate_from(&MessageId::from("m3"), None),
            TranscriptChange::Unchanged,
            "a repeated commit changes nothing"
        );
        let _ = transcript.materialize(history.into_iter().rev().collect(), since);
        assert_eq!(
            transcript.len(),
            2,
            "a read started before the commit cannot restore trimmed messages"
        );
        // A boundary older than the loaded window trims everything only when
        // the session's own marker names it.
        assert_eq!(
            transcript.truncate_from(&MessageId::from("older"), None),
            TranscriptChange::Unchanged
        );
        let older = MessageId::from("older");
        assert_eq!(
            transcript.truncate_from(&older, Some(&older)),
            TranscriptChange::Removed { rows: 0..2 }
        );
    }

    #[test]
    fn a_commit_during_a_load_keeps_the_cut_out_of_that_read() {
        let history = vec![
            message("m1", Role::User, "a"),
            message("m2", Role::Assistant, "b"),
            message("m3", Role::User, "c"),
            message("m4", Role::Assistant, "d"),
        ];
        let mut transcript = Transcript::new();
        let since = transcript.revision();
        assert_eq!(
            transcript.truncate_from(&MessageId::from("m3"), None),
            TranscriptChange::Unchanged,
            "nothing is loaded yet"
        );
        let _ = transcript.materialize(history.clone(), since);
        let ids: Vec<&str> = transcript
            .messages()
            .iter()
            .map(|message| message.info.id.as_str())
            .collect();
        assert_eq!(ids, ["m1", "m2"], "the read started before the commit");
        // A read requested after the commit is the server's own answer.
        let later = transcript.revision();
        let _ = transcript.materialize(history, later);
        assert_eq!(transcript.len(), 4);
    }

    #[test]
    fn precommit_page_without_the_boundary_has_no_authority() {
        let mut transcript = Transcript::new();
        let since = transcript.revision();
        let _ = transcript.truncate_from(&MessageId::from("boundary-outside-latest-page"), None);
        let page = vec![message("suffix", Role::Assistant, "deleted suffix")];
        assert_eq!(
            transcript.materialize(page.clone(), since),
            TranscriptChange::NeedsReconcile
        );
        assert!(transcript.is_empty());
        let fresh_since = transcript.revision();
        let fresh = vec![message("authoritative", Role::User, "new prompt")];
        let _ = transcript.materialize(fresh, fresh_since);
        assert_eq!(
            transcript.prepend_page(page.clone(), since),
            TranscriptChange::NeedsReconcile
        );
        assert_eq!(
            transcript.materialize(page, since),
            TranscriptChange::NeedsReconcile
        );
        assert_eq!(transcript.len(), 1);
        assert_eq!(
            transcript
                .message(0)
                .expect("the authoritative row remains")
                .info
                .id
                .as_str(),
            "authoritative"
        );
    }

    #[test]
    fn a_subagent_call_learns_its_child_and_reads_keep_it() {
        let call = |child: Option<&str>, status: ToolStatus| Part {
            id: PartId::from("call"),
            content: PartContent::Tool {
                tool: "subagent".to_owned(),
                status,
                title: None,
                output: None,
                elided: None,
                child: child.map(SessionId::from),
                facts: Box::default(),
            },
            time: PartTime::default(),
        };
        let mut step = message("m", Role::Assistant, "");
        step.parts = vec![call(None, ToolStatus::Running)];
        let mut transcript = Transcript::from_history(vec![step.clone()]);
        let event = TranscriptEvent::ToolChild {
            message_id: MessageId::from("m"),
            part_id: PartId::from("call"),
            child: SessionId::from("ses_child"),
        };
        assert_eq!(
            transcript.apply(event.clone()),
            TranscriptChange::Updated { row: 0 }
        );
        assert_eq!(transcript.apply(event), TranscriptChange::Unchanged);
        assert_eq!(
            transcript.apply(TranscriptEvent::ToolChild {
                message_id: MessageId::from("m"),
                part_id: PartId::from("missing"),
                child: SessionId::from("ses_child"),
            }),
            TranscriptChange::NeedsReconcile
        );
        // A read of the running call has no child; the live one stays.
        let since = transcript.revision();
        let _ = transcript.materialize(vec![step], since);
        assert_eq!(
            transcript.messages()[0].parts[0],
            call(Some("ses_child"), ToolStatus::Running)
        );
    }
}
