//! Client-owned transcript scroll positions keyed by runtime and session.

use std::collections::HashMap;

use crate::ids::{MessageId, RuntimeId, SessionId};

/// Where a reader left a transcript.
///
/// The anchor is the first visible message and the logical-pixel offset of
/// the viewport top inside it. A message identity survives prepends and
/// streaming; a row index would not.
#[derive(Clone, Debug, PartialEq)]
pub enum ScrollPosition {
    /// The reader is at the end and growth keeps following it.
    FollowingTail,
    Anchored {
        message_id: MessageId,
        offset_in_message: f32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScrollKey {
    pub runtime: RuntimeId,
    pub session: SessionId,
}

/// Saved positions for every visited transcript.
#[derive(Clone, Debug, Default)]
pub struct ScrollMemory {
    positions: HashMap<ScrollKey, ScrollPosition>,
}

impl ScrollMemory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn save(&mut self, key: ScrollKey, position: ScrollPosition) {
        self.positions.insert(key, position);
    }

    /// The saved position; unvisited transcripts open at the tail.
    pub fn position(&self, key: &ScrollKey) -> &ScrollPosition {
        self.positions
            .get(key)
            .unwrap_or(&ScrollPosition::FollowingTail)
    }

    /// Forgets positions of a runtime, as when its data is reset.
    pub fn forget_runtime(&mut self, runtime: &RuntimeId) {
        self.positions.retain(|key, _| &key.runtime != runtime);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unvisited_transcripts_follow_the_tail() {
        let memory = ScrollMemory::new();
        let key = ScrollKey {
            runtime: RuntimeId::from("r"),
            session: SessionId::from("s"),
        };
        assert_eq!(memory.position(&key), &ScrollPosition::FollowingTail);
    }

    #[test]
    fn positions_are_per_runtime() {
        let mut memory = ScrollMemory::new();
        let key = |runtime: &str| ScrollKey {
            runtime: RuntimeId::from(runtime),
            session: SessionId::from("s"),
        };
        let anchored = ScrollPosition::Anchored {
            message_id: MessageId::from("m4"),
            offset_in_message: 37.5,
        };
        memory.save(key("a"), anchored.clone());
        assert_eq!(memory.position(&key("a")), &anchored);
        assert_eq!(memory.position(&key("b")), &ScrollPosition::FollowingTail);
        memory.forget_runtime(&RuntimeId::from("a"));
        assert_eq!(memory.position(&key("a")), &ScrollPosition::FollowingTail);
    }
}
