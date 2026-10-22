//! Ordered session transcripts with stable message and part identities.
//!
//! Server order is authoritative: history arrives in order, live messages
//! append, older pages prepend. Identifier spelling never reorders anything.
//! Every mutation reports which rows changed so the renderer can update one
//! row for a text delta without rebuilding the list.

use std::collections::HashMap;
use std::ops::Range;

use crate::ids::{MessageId, PartId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    User,
    Assistant,
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
    },
    File {
        filename: String,
        mime: String,
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

/// Message metadata without its parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageInfo {
    pub id: MessageId,
    pub role: Role,
    pub created: i64,
    /// Present once an assistant message finished.
    pub completed: Option<i64>,
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

    /// Increments on every observable mutation.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replaces the transcript with authoritative history.
    pub fn replace(&mut self, messages: Vec<Message>) -> TranscriptChange {
        if messages == self.messages {
            return TranscriptChange::Unchanged;
        }
        self.messages = messages;
        self.reindex(0);
        self.revision += 1;
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
        self.revision += 1;
        TranscriptChange::Inserted { rows: 0..count }
    }

    /// Appends a message, or updates it in place when already held.
    pub fn push(&mut self, message: Message) -> TranscriptChange {
        if let Some(row) = self.row_of(&message.info.id) {
            if self.messages[row] == message {
                return TranscriptChange::Unchanged;
            }
            self.messages[row] = message;
            self.revision += 1;
            return TranscriptChange::Updated { row };
        }
        let row = self.messages.len();
        self.row_by_id.insert(message.info.id.clone(), row);
        self.messages.push(message);
        self.revision += 1;
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
                    self.revision += 1;
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
                    Some(existing) => *existing = part,
                    None => parts.push(part),
                }
                self.revision += 1;
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
                match parts.iter_mut().find(|part| part.id == part_id) {
                    Some(Part {
                        content: PartContent::Text { text } | PartContent::Reasoning { text },
                        ..
                    }) => text.push_str(&delta),
                    Some(_) => return TranscriptChange::NeedsReconcile,
                    None => parts.push(Part::text(part_id, delta)),
                }
                self.revision += 1;
                TranscriptChange::Updated { row }
            }
            TranscriptEvent::MessageRemoved { message_id } => {
                let Some(row) = self.row_of(&message_id) else {
                    return TranscriptChange::Unchanged;
                };
                self.messages.remove(row);
                self.row_by_id.remove(&message_id);
                self.reindex(row);
                self.revision += 1;
                TranscriptChange::Removed { rows: row..row + 1 }
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
                self.revision += 1;
                TranscriptChange::Updated { row }
            }
        }
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
            },
            parts: vec![Part::text(format!("{id}-p0").as_str(), text)],
        }
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
