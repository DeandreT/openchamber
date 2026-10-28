//! Client-owned composer drafts keyed by their captured identity.
//!
//! A draft belongs to the runtime, actual directory and session (or
//! new-session draft) that was current when it was typed. Switching sessions
//! saves the outgoing draft under that captured key, so a late save can
//! never land in whichever session is selected by then.

use std::collections::HashMap;
use std::ops::Range;

use crate::ids::{DraftId, RuntimeId, SessionId};
use crate::path;

/// What a draft will be sent to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DraftTarget {
    Session(SessionId),
    /// A new session that does not exist yet.
    NewSession(DraftId),
}

/// The identity a draft is stored under.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DraftKey {
    runtime: RuntimeId,
    directory: String,
    target: DraftTarget,
}

impl DraftKey {
    /// Builds a key from the session's real directory.
    ///
    /// Returns `None` when the directory is unusable.
    pub fn new(runtime: RuntimeId, directory: &str, target: DraftTarget) -> Option<Self> {
        Some(Self {
            runtime,
            directory: path::normalize(directory)?,
            target,
        })
    }

    pub fn runtime(&self) -> &RuntimeId {
        &self.runtime
    }

    pub fn directory(&self) -> &str {
        &self.directory
    }

    pub fn target(&self) -> &DraftTarget {
        &self.target
    }
}

/// Composer text and selection; offsets are UTF-8 byte positions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub text: String,
    pub selection: Range<usize>,
}

impl Draft {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        Self {
            text,
            selection: end..end,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// All unsent drafts of the client.
///
/// Retention never discards unsent content: only an explicit clear or send
/// removes a non-empty draft.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DraftStore {
    drafts: HashMap<DraftKey, Draft>,
}

impl DraftStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `draft` under `key`; an empty draft clears the entry.
    pub fn save(&mut self, key: DraftKey, draft: Draft) {
        if draft.is_empty() {
            self.drafts.remove(&key);
        } else {
            self.drafts.insert(key, draft);
        }
    }

    pub fn get(&self, key: &DraftKey) -> Option<&Draft> {
        self.drafts.get(key)
    }

    /// Borrow every retained draft for a complete persistence snapshot.
    pub fn iter(&self) -> impl Iterator<Item = (&DraftKey, &Draft)> {
        self.drafts.iter()
    }

    /// Removes and returns the draft, as a send does.
    pub fn take(&mut self, key: &DraftKey) -> Option<Draft> {
        self.drafts.remove(key)
    }

    /// Moves a new-session draft to the session it became.
    ///
    /// An existing draft under the session key wins, so a newer draft is
    /// never overwritten by a materialization that arrives late.
    pub fn materialize(&mut self, from: &DraftKey, session_id: SessionId) -> Option<DraftKey> {
        let draft = self.drafts.remove(from)?;
        let key = DraftKey {
            runtime: from.runtime.clone(),
            directory: from.directory.clone(),
            target: DraftTarget::Session(session_id),
        };
        self.drafts.entry(key.clone()).or_insert(draft);
        Some(key)
    }

    pub fn len(&self) -> usize {
        self.drafts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.drafts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(runtime: &str, directory: &str, session: &str) -> DraftKey {
        DraftKey::new(
            RuntimeId::from(runtime),
            directory,
            DraftTarget::Session(SessionId::from(session)),
        )
        .expect("directory is usable")
    }

    #[test]
    fn drafts_stay_with_their_captured_runtime_and_directory() {
        let mut store = DraftStore::new();
        store.save(key("a", "/p/", "s1"), Draft::new("hello"));
        assert_eq!(
            store
                .get(&key("a", "/p", "s1"))
                .map(|draft| draft.text.as_str()),
            Some("hello")
        );
        assert_eq!(store.get(&key("b", "/p", "s1")), None);
        assert_eq!(store.get(&key("a", "/q", "s1")), None);
    }

    #[test]
    fn saving_an_empty_draft_clears_it() {
        let mut store = DraftStore::new();
        store.save(key("a", "/p", "s1"), Draft::new("x"));
        store.save(key("a", "/p", "s1"), Draft::default());
        assert!(store.is_empty());
    }

    #[test]
    fn materializing_keeps_a_newer_session_draft() {
        let mut store = DraftStore::new();
        let new = DraftKey::new(
            RuntimeId::from("a"),
            "/p",
            DraftTarget::NewSession(DraftId::from("d1")),
        )
        .expect("directory is usable");
        store.save(new.clone(), Draft::new("old"));
        store.save(key("a", "/p", "s1"), Draft::new("newer"));
        let moved = store
            .materialize(&new, SessionId::from("s1"))
            .expect("draft existed");
        assert_eq!(
            store.get(&moved).map(|draft| draft.text.as_str()),
            Some("newer")
        );
        assert_eq!(store.get(&new), None);
    }
}
