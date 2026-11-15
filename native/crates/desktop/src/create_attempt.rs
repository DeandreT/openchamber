//! Durable root-session creation ownership, before a session-owned send exists.

use std::collections::HashMap;

use openchamber_native_core::draft::{Draft, DraftKey};
use openchamber_native_core::ids::AttachmentId;
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
    /// Prepared resources retained by the first-send payload, independent of rollback text.
    pub resources: Vec<AttachmentId>,
    pub native_config: Option<crate::native_agent::CreationConfig>,
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

    pub(crate) fn bind_native_execution(
        &mut self,
        key: &SessionKey,
        model: openchamber_native_core::send_config::ModelChoice,
    ) -> bool {
        let Some(attempt) = self.entries.get_mut(key) else {
            return false;
        };
        if attempt.phase != CreationPhase::Confirmed {
            return false;
        }
        attempt.prompt.model = Some(model);
        true
    }

    pub(crate) fn require_native_correction(&mut self, key: &SessionKey) -> bool {
        let Some(config) = self
            .entries
            .get_mut(key)
            .and_then(|attempt| attempt.native_config.as_mut())
        else {
            return false;
        };
        if !config.prepare_after_session {
            return false;
        }
        config.correction_required = true;
        true
    }

    /// Only an explicit pre-dispatch retry may accept corrected displayed choices.
    /// The captured request, message, agent, directory and resources never change.
    pub(crate) fn correct_native_config(
        &mut self,
        key: &SessionKey,
        config: Vec<(String, String)>,
        complete: bool,
    ) -> bool {
        if config.len() > crate::native_agent::CONFIG_CATALOG_LIMIT {
            return false;
        }
        let Some(attempt) = self
            .entries
            .get_mut(key)
            .filter(|attempt| attempt.phase == CreationPhase::Confirmed)
        else {
            return false;
        };
        let Some(prior) = &attempt.native_config else {
            return false;
        };
        if !prior.prepare_after_session || !prior.correction_required {
            return false;
        }
        let mut corrected = prior.clone();
        corrected.config = if complete {
            config
        } else {
            let mut merged: Vec<_> = prior
                .config
                .iter()
                .filter(|(id, _)| !config.iter().any(|(known, _)| known == id))
                .cloned()
                .collect();
            merged.extend(config);
            merged
        };
        corrected.correction_required = false;
        if !corrected.validate() {
            return false;
        }
        attempt.native_config = Some(corrected);
        true
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
