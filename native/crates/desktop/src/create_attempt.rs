//! Durable root-session creation ownership, before a session-owned send exists.

use std::collections::HashMap;

use openchamber_native_core::draft::{Draft, DraftKey};
use openchamber_native_transport::prompt::Prompt;
use openchamber_native_transport::session_create::CreateSession;

use crate::send_attempt::SessionKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CreationPhase {
    Preparing,
    Creating,
    Uncertain,
    Confirming,
    Confirmed,
    Mismatch,
    Deleted,
}

#[derive(Clone)]
pub(crate) struct CreateAttempt {
    pub source: DraftKey,
    pub project: String,
    pub request: CreateSession,
    pub draft: Draft,
    /// Original configuration, including Auto, independent of creation defaults.
    pub prompt: Prompt,
    pub created: i64,
    pub phase: CreationPhase,
    /// Local navigation generation. Restored attempts never select a view.
    pub navigation: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreationCheck {
    Checking,
    NotObserved,
    Failed,
    Mismatch,
}

#[derive(Default)]
pub(crate) struct CreateAttempts {
    entries: HashMap<SessionKey, CreateAttempt>,
    pub checks: HashMap<SessionKey, CreationCheck>,
}

impl CreateAttempts {
    pub(crate) fn can_begin(&self, source: &DraftKey) -> bool {
        !self
            .entries
            .values()
            .any(|attempt| &attempt.source == source && attempt.phase != CreationPhase::Deleted)
    }

    pub(crate) fn insert(&mut self, key: SessionKey, attempt: CreateAttempt) -> bool {
        if self.entries.contains_key(&key) || !self.can_begin(&attempt.source) {
            return false;
        }
        self.entries.insert(key, attempt);
        true
    }

    pub(crate) fn get(&self, key: &SessionKey) -> Option<&CreateAttempt> {
        self.entries.get(key)
    }

    pub(crate) fn phase(&mut self, key: &SessionKey, phase: CreationPhase) -> bool {
        let Some(attempt) = self.entries.get_mut(key) else {
            return false;
        };
        if attempt.phase == CreationPhase::Deleted {
            return false;
        }
        attempt.phase = phase;
        true
    }

    pub(crate) fn remove(&mut self, key: &SessionKey) -> Option<CreateAttempt> {
        self.checks.remove(key);
        self.entries.remove(key)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&SessionKey, &CreateAttempt)> {
        self.entries.iter()
    }

    pub(crate) fn snapshot(&self) -> Vec<(SessionKey, CreateAttempt)> {
        self.iter()
            .map(|(key, attempt)| (key.clone(), attempt.clone()))
            .collect()
    }

    pub(crate) fn restore(entries: Vec<(SessionKey, CreateAttempt)>) -> Self {
        let mut restored = Self::default();
        for (key, mut attempt) in entries {
            attempt.navigation = None;
            if matches!(
                attempt.phase,
                CreationPhase::Preparing | CreationPhase::Creating
            ) {
                attempt.phase = CreationPhase::Uncertain;
            }
            restored.entries.insert(key, attempt);
        }
        restored
    }
}

#[cfg(test)]
#[path = "create_attempt_tests.rs"]
pub(crate) mod tests;
