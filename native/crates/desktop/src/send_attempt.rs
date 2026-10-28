//! Window-lifetime mutation ownership, independent of current selection.

use std::collections::{HashMap, HashSet};

use openchamber_native_core::draft::{Draft, DraftKey};
use openchamber_native_core::ids::{MessageId, RuntimeId, SessionId};
use openchamber_native_core::transcript::{
    Message, Role, Transcript, TranscriptChange, TranscriptEvent,
};
use openchamber_native_transport::prompt::{PromptAdmission, TextPrompt};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionKey {
    pub runtime: RuntimeId,
    pub session: SessionId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    Preparing,
    Sending { observed: bool },
    Admitted,
    Uncertain,
}

#[derive(Clone)]
pub(crate) struct SendAttempt {
    pub owner: DraftKey,
    pub draft: Draft,
    pub prompt: TextPrompt,
    pub optimistic: Message,
    pub admission: Admission,
}

#[derive(Default)]
pub(crate) struct SendAttempts {
    attempts: HashMap<SessionKey, SendAttempt>,
    active: HashMap<SessionKey, ActiveMutation>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveMutation {
    Send,
    Abort,
}

pub(crate) enum HistoryMerge {
    Latest,
    Earlier,
}

impl SendAttempts {
    /// Captures durable prompts only. Abort tasks have no replayable payload.
    pub(crate) fn snapshot(&self) -> Vec<(SessionKey, SendAttempt)> {
        self.attempts
            .iter()
            .map(|(key, attempt)| (key.clone(), attempt.clone()))
            .collect()
    }

    /// Restores a validated journal into a fresh store without starting tasks.
    /// Interrupted sends remain blocked until their exact identity reconciles.
    pub(crate) fn from_snapshot(
        snapshot: Vec<(SessionKey, SendAttempt)>,
    ) -> Result<Self, &'static str> {
        use openchamber_native_core::draft::DraftTarget;

        let mut restored = Self::default();
        let mut identities = HashSet::new();
        for (key, mut attempt) in snapshot {
            if attempt.owner.runtime() != &key.runtime
                || attempt.owner.target() != &DraftTarget::Session(key.session.clone())
                || restored.attempts.contains_key(&key)
                || !identities.insert((key.runtime.clone(), attempt.prompt.message_id.clone()))
            {
                return Err("invalid or duplicate attempt owner or identity");
            }
            if matches!(
                attempt.admission,
                Admission::Preparing | Admission::Sending { .. }
            ) {
                attempt.admission = Admission::Uncertain;
            }
            restored.attempts.insert(key, attempt);
        }
        Ok(restored)
    }

    pub(crate) fn can_send(&self, key: &SessionKey) -> bool {
        !self.attempts.contains_key(key) && !self.active.contains_key(key)
    }

    pub(crate) fn begin(&mut self, key: SessionKey, attempt: SendAttempt) -> bool {
        if !self.can_send(&key) {
            return false;
        }
        self.active.insert(key.clone(), ActiveMutation::Send);
        self.attempts.insert(key, attempt);
        true
    }

    pub(crate) fn begin_abort(&mut self, key: SessionKey) -> bool {
        use std::collections::hash_map::Entry;
        let Entry::Vacant(entry) = self.active.entry(key) else {
            return false;
        };
        entry.insert(ActiveMutation::Abort);
        true
    }

    pub(crate) fn is_active(&self, key: &SessionKey) -> bool {
        self.active.contains_key(key)
    }

    pub(crate) fn prepared(&mut self, key: &SessionKey, id: &MessageId) -> bool {
        let Some(attempt) = self.attempts.get_mut(key) else {
            return false;
        };
        if attempt.prompt.message_id != *id || attempt.admission != Admission::Preparing {
            return false;
        }
        attempt.admission = Admission::Sending { observed: false };
        true
    }

    pub(crate) fn begin_retry(&mut self, key: &SessionKey) -> bool {
        if !self
            .attempts
            .get(key)
            .is_some_and(|attempt| attempt.admission == Admission::Uncertain)
            || self.active.contains_key(key)
        {
            return false;
        }
        self.active.insert(key.clone(), ActiveMutation::Send);
        true
    }

    pub(crate) fn confirm_pending(&mut self, key: &SessionKey, id: &MessageId) -> bool {
        let Some(attempt) = self.attempts.get_mut(key) else {
            return false;
        };
        if attempt.prompt.message_id != *id {
            return false;
        }
        attempt.admission = if self.active.get(key) == Some(&ActiveMutation::Send) {
            Admission::Sending { observed: true }
        } else {
            Admission::Admitted
        };
        true
    }

    pub(crate) fn finish_abort(&mut self, key: &SessionKey) {
        if self.active.get(key) == Some(&ActiveMutation::Abort) {
            self.active.remove(key);
        }
    }

    pub(crate) fn attempt(&self, key: &SessionKey) -> Option<&SendAttempt> {
        self.attempts.get(key)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&SessionKey, &SendAttempt)> {
        self.attempts.iter()
    }

    pub(crate) fn apply_event(
        &mut self,
        key: &SessionKey,
        transcript: &mut Transcript,
        event: TranscriptEvent,
    ) -> TranscriptChange {
        let mut cleared = TranscriptChange::Unchanged;
        if let TranscriptEvent::MessageUpdated(info) = &event
            && info.role == Role::User
            && self.observe(key, &info.id)
        {
            cleared = transcript.apply(TranscriptEvent::PartsReplaced {
                message_id: info.id.clone(),
                parts: Vec::new(),
            });
        }
        let change = transcript.apply(event);
        if change == TranscriptChange::Unchanged
            && matches!(cleared, TranscriptChange::Updated { .. })
        {
            cleared
        } else {
            change
        }
    }

    /// Replaces an optimistic message before core's part-preserving history
    /// merge. Missing IDs preserve unresolved optimistic content and admission.
    pub(crate) fn merge_history(
        &mut self,
        key: &SessionKey,
        transcript: &mut Transcript,
        messages: Vec<Message>,
        since: u64,
        merge: HistoryMerge,
    ) -> TranscriptChange {
        let incoming = self.attempt(key).and_then(|attempt| {
            messages.iter().find(|message| {
                message.info.id == attempt.prompt.message_id && message.info.role == Role::User
            })
        });
        let replacement = incoming
            .cloned()
            .filter(|message| self.observe(key, &message.info.id));
        if let Some(authoritative) = &replacement
            && matches!(merge, HistoryMerge::Earlier)
        {
            // A retained optimistic row can sit after newer history. Place
            // its authoritative record in this page's order, while keeping
            // every other held row and core's newer-deletion protection.
            let held: HashSet<_> = transcript
                .messages()
                .iter()
                .map(|message| &message.info.id)
                .collect();
            let mut combined: Vec<_> = messages
                .into_iter()
                .filter(|message| {
                    message.info.id == authoritative.info.id || !held.contains(&message.info.id)
                })
                .collect();
            combined.extend(
                transcript
                    .messages()
                    .iter()
                    .filter(|message| message.info.id != authoritative.info.id)
                    .cloned(),
            );
            let changed = transcript.materialize(combined, since) != TranscriptChange::Unchanged;
            let replaced = transcript.row_of(&authoritative.info.id).is_some()
                && transcript.push(authoritative.clone()) != TranscriptChange::Unchanged;
            return if changed || replaced {
                TranscriptChange::Reset
            } else {
                TranscriptChange::Unchanged
            };
        }
        let replaced = replacement
            .is_some_and(|message| transcript.push(message) != TranscriptChange::Unchanged);
        let change = match merge {
            HistoryMerge::Latest => transcript.materialize(messages, since),
            HistoryMerge::Earlier => transcript.prepend_page(messages, since),
        };
        let restored = self.attempt(key).is_some_and(|attempt| {
            if matches!(
                attempt.admission,
                Admission::Preparing | Admission::Sending { observed: true }
            ) || transcript.row_of(&attempt.prompt.message_id).is_some()
            {
                return false;
            }
            transcript.push(attempt.optimistic.clone()) != TranscriptChange::Unchanged
        });
        if replaced || restored {
            TranscriptChange::Reset
        } else {
            change
        }
    }

    /// Only exact authoritative identity proves admission. The first such
    /// record replaces local content, including plugin-transformed text.
    pub(crate) fn observe(&mut self, key: &SessionKey, id: &MessageId) -> bool {
        let Some(attempt) = self.attempts.get_mut(key) else {
            return false;
        };
        if attempt.prompt.message_id != *id {
            return false;
        }
        match attempt.admission {
            Admission::Preparing => {
                self.attempts.remove(key);
                self.active.remove(key);
                true
            }
            Admission::Sending { observed: true } => false,
            Admission::Sending { observed: false } => {
                attempt.admission = Admission::Sending { observed: true };
                true
            }
            Admission::Admitted | Admission::Uncertain => {
                if self.active.get(key) == Some(&ActiveMutation::Send) {
                    attempt.admission = Admission::Sending { observed: true };
                } else {
                    self.attempts.remove(key);
                }
                true
            }
        }
    }

    /// Returns the attempt only for a definite rejection needing rollback.
    /// An observed message wins over a late or contradictory HTTP failure.
    pub(crate) fn finish(
        &mut self,
        key: &SessionKey,
        outcome: Result<(), PromptAdmission>,
    ) -> Option<SendAttempt> {
        if self.active.get(key) == Some(&ActiveMutation::Send) {
            self.active.remove(key);
        }
        let attempt = self.attempts.get_mut(key)?;
        if attempt.admission == (Admission::Sending { observed: true }) {
            self.attempts.remove(key);
            return None;
        }
        if attempt.admission == Admission::Uncertain && outcome.is_err() {
            return None;
        }
        match outcome {
            Ok(()) => attempt.admission = Admission::Admitted,
            Err(PromptAdmission::Unknown) => attempt.admission = Admission::Uncertain,
            Err(PromptAdmission::NotAttempted | PromptAdmission::Rejected) => {
                return self.attempts.remove(key);
            }
        }
        None
    }
}

/// Web useComposerDraft.restoreDraft semantics, retaining the newer selection
/// when appending failed text to a draft that was edited after dispatch.
pub(crate) fn restore_failed(current: Option<&Draft>, failed: Draft) -> Draft {
    match current {
        Some(current) if !current.text.is_empty() && current.text != failed.text => Draft {
            text: format!("{}\n\n{}", current.text, failed.text),
            selection: current.selection.clone(),
        },
        Some(current) if current.text == failed.text => current.clone(),
        _ => failed,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use openchamber_native_core::draft::DraftTarget;
    use openchamber_native_core::ids::PartId;
    use openchamber_native_core::send_config::ModelChoice;
    use openchamber_native_core::transcript::{MessageInfo, Part};

    pub(crate) fn key(runtime: &str, session: &str) -> SessionKey {
        SessionKey {
            runtime: RuntimeId::from(runtime),
            session: SessionId::from(session),
        }
    }

    pub(crate) fn attempt(key: &SessionKey) -> SendAttempt {
        let draft = Draft {
            text: "submitted text".to_owned(),
            selection: 2..6,
        };
        let message_id = MessageId::from("msg_bcfe56800001AbCdEf01234567");
        SendAttempt {
            owner: DraftKey::new(
                key.runtime.clone(),
                "/project",
                DraftTarget::Session(key.session.clone()),
            )
            .expect("test directory is absolute"),
            optimistic: Message {
                info: MessageInfo {
                    id: message_id.clone(),
                    role: Role::User,
                    created: 1,
                    completed: None,
                    notice: None,
                    assistant: None,
                    compaction: None,
                },
                parts: vec![Part::text(PartId::from("local-part"), draft.text.clone())],
            },
            draft: draft.clone(),
            prompt: TextPrompt {
                message_id,
                text: draft.text,
                model: Some(ModelChoice {
                    provider_id: "provider".to_owned(),
                    model_id: "model".to_owned(),
                    variant: Some("effort".to_owned()),
                }),
                agent: Some("agent".to_owned()),
                delivery: None,
            },
            admission: Admission::Sending { observed: false },
        }
    }

    #[test]
    fn acknowledgement_and_authoritative_history_deduplicate_in_both_orders() {
        for event_first in [false, true] {
            let key = key("runtime", "session");
            let mut attempts = SendAttempts::default();
            let local = attempt(&key);
            let mut authoritative = local.optimistic.clone();
            authoritative.parts = vec![Part::text(PartId::from("server-part"), "plugin prepared")];
            let mut transcript = Transcript::new();
            let _ = transcript.push(local.optimistic.clone());
            assert!(attempts.begin(key.clone(), local));
            if !event_first {
                attempts.finish(&key, Ok(()));
            }
            let _ = attempts.merge_history(
                &key,
                &mut transcript,
                vec![authoritative.clone()],
                0,
                HistoryMerge::Latest,
            );
            if event_first {
                attempts.finish(&key, Ok(()));
            }
            let _ = attempts.merge_history(
                &key,
                &mut transcript,
                vec![authoritative.clone()],
                0,
                HistoryMerge::Latest,
            );
            assert!(
                attempts.can_send(&key),
                "exact authority releases the attempt after response"
            );
            assert_eq!(
                transcript.len(),
                1,
                "history and optimism share one identity"
            );
            assert_eq!(transcript.message(0), Some(&authoritative));
        }
    }

    #[test]
    fn acknowledgement_and_event_order_keep_one_authoritative_user_message() {
        for event_first in [false, true] {
            let key = key("runtime", "session");
            let mut attempts = SendAttempts::default();
            let local = attempt(&key);
            let mut authoritative = local.optimistic.clone();
            authoritative.parts = vec![Part::text(PartId::from("server-part"), "prepared")];
            let mut transcript = Transcript::new();
            let _ = transcript.push(local.optimistic.clone());
            attempts.begin(key.clone(), local);
            if !event_first {
                attempts.finish(&key, Ok(()));
            }
            for _ in 0..2 {
                let _ = attempts.apply_event(
                    &key,
                    &mut transcript,
                    TranscriptEvent::MessageUpdated(authoritative.info.clone()),
                );
                let _ = attempts.apply_event(
                    &key,
                    &mut transcript,
                    TranscriptEvent::PartsReplaced {
                        message_id: authoritative.info.id.clone(),
                        parts: authoritative.parts.clone(),
                    },
                );
            }
            if event_first {
                attempts.finish(&key, Ok(()));
            }
            assert_eq!(transcript.len(), 1);
            assert_eq!(transcript.message(0), Some(&authoritative));
            assert!(attempts.can_send(&key));
        }
    }

    #[test]
    fn earlier_history_places_a_retained_attempt_before_newer_messages() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        let local = attempt(&key);
        let mut authoritative = local.optimistic.clone();
        authoritative.parts = vec![Part::text(PartId::from("server-part"), "prepared")];
        let mut newer = authoritative.clone();
        newer.info.id = MessageId::from("msg_newer");
        newer.info.created = 10;
        let mut older = authoritative.clone();
        older.info.id = MessageId::from("msg_older");
        older.info.created = 0;
        let mut transcript = Transcript::from_history(vec![newer.clone()]);
        let _ = transcript.push(local.optimistic.clone());
        let since = transcript.revision();
        attempts.begin(key.clone(), local);
        attempts.finish(&key, Err(PromptAdmission::Unknown));
        let _ = attempts.merge_history(
            &key,
            &mut transcript,
            vec![older.clone(), authoritative.clone()],
            since,
            HistoryMerge::Earlier,
        );
        assert_eq!(
            transcript.messages(),
            &[older, authoritative.clone(), newer]
        );
        assert!(attempts.can_send(&key));
        let _ = attempts.merge_history(
            &key,
            &mut transcript,
            vec![authoritative],
            since,
            HistoryMerge::Earlier,
        );
        assert_eq!(
            transcript.len(),
            3,
            "replayed pages cannot duplicate the admitted prompt"
        );
    }

    #[test]
    fn event_before_failed_response_prevents_false_rollback() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        let local = attempt(&key);
        let id = local.prompt.message_id.clone();
        attempts.begin(key.clone(), local);
        assert!(attempts.observe(&key, &id));
        assert!(
            !attempts.can_send(&key),
            "the mutation chain still owns its slot"
        );
        assert!(!attempts.begin_abort(key.clone()));
        assert!(
            attempts
                .finish(&key, Err(PromptAdmission::Rejected))
                .is_none()
        );
        assert!(attempts.can_send(&key));
    }

    #[test]
    fn uncertain_admission_keeps_text_id_and_lock_until_exact_authority() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        let local = attempt(&key);
        let id = local.prompt.message_id.clone();
        let mut transcript = Transcript::new();
        let _ = transcript.push(local.optimistic.clone());
        attempts.begin(key.clone(), local);
        attempts.finish(&key, Err(PromptAdmission::Unknown));
        assert!(!attempts.can_send(&key));
        assert!(!attempts.begin(key.clone(), attempt(&key)));
        assert!(!attempts.observe(&key, &MessageId::from("different")));
        let since = transcript.revision();
        let _ = attempts.merge_history(
            &key,
            &mut transcript,
            Vec::new(),
            since,
            HistoryMerge::Latest,
        );
        assert_eq!(transcript.len(), 1, "absence cannot discard uncertain text");
        let pending = attempts
            .attempt(&key)
            .expect("uncertain attempt is retained");
        assert_eq!(pending.admission, Admission::Uncertain);
        assert_eq!(pending.draft.selection, 2..6);
        assert_eq!(pending.prompt.text, "submitted text");
        assert_eq!(pending.prompt.message_id, id);
        assert!(attempts.observe(&key, &id));
        assert!(attempts.can_send(&key));
    }

    #[test]
    fn preparation_waits_for_its_exact_durable_identity() {
        let key = key("runtime", "session");
        let mut local = attempt(&key);
        local.admission = Admission::Preparing;
        let id = local.prompt.message_id.clone();
        let mut attempts = SendAttempts::default();
        assert!(attempts.begin(key.clone(), local));
        let mut transcript = Transcript::new();
        let _ = attempts.merge_history(&key, &mut transcript, vec![], 0, HistoryMerge::Latest);
        assert!(
            transcript.is_empty(),
            "an uncommitted preparation is not an optimistic send"
        );
        assert!(!attempts.prepared(&key, &MessageId::from("msg_other")));
        let recovered = SendAttempts::from_snapshot(attempts.snapshot()).expect("valid snapshot");
        assert_eq!(
            recovered.attempt(&key).expect("retained").admission,
            Admission::Uncertain
        );
        assert!(attempts.prepared(&key, &id));
        assert_eq!(
            attempts.attempt(&key).expect("retained").admission,
            Admission::Sending { observed: false }
        );
    }

    #[test]
    fn a_failed_retry_cannot_reject_the_original_uncertain_send() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        attempts.begin(key.clone(), attempt(&key));
        attempts.finish(&key, Err(PromptAdmission::Unknown));
        let id = attempts
            .attempt(&key)
            .expect("retained")
            .prompt
            .message_id
            .clone();
        assert!(attempts.begin_retry(&key));
        assert!(!attempts.begin_retry(&key));
        assert!(
            attempts
                .finish(&key, Err(PromptAdmission::Rejected))
                .is_none()
        );
        assert_eq!(
            attempts.attempt(&key).expect("retained").admission,
            Admission::Uncertain
        );
        assert_eq!(
            attempts.attempt(&key).expect("retained").prompt.message_id,
            id
        );
    }

    #[test]
    fn observing_an_active_retry_keeps_ownership_until_its_response() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        attempts.begin(key.clone(), attempt(&key));
        attempts.finish(&key, Err(PromptAdmission::Unknown));
        let id = attempts
            .attempt(&key)
            .expect("retained")
            .prompt
            .message_id
            .clone();
        assert!(attempts.begin_retry(&key));
        assert!(attempts.observe(&key, &id));
        assert!(
            attempts.attempt(&key).is_some(),
            "the response still owns the attempt"
        );
        assert!(!attempts.can_send(&key));
        assert!(
            attempts
                .finish(&key, Err(PromptAdmission::Unknown))
                .is_none()
        );
        assert!(
            attempts.can_send(&key),
            "completion releases the observed retry"
        );
    }

    #[test]
    fn observing_during_abort_does_not_create_a_phantom_send() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        attempts.begin(key.clone(), attempt(&key));
        attempts.finish(&key, Err(PromptAdmission::Unknown));
        let id = attempts
            .attempt(&key)
            .expect("retained")
            .prompt
            .message_id
            .clone();
        assert!(attempts.begin_abort(key.clone()));
        assert!(attempts.observe(&key, &id));
        assert!(
            attempts.attempt(&key).is_none(),
            "the authoritative message resolves the prompt"
        );
        assert!(!attempts.can_send(&key), "the abort still owns its slot");
        attempts.finish_abort(&key);
        assert!(attempts.can_send(&key));
    }

    #[test]
    fn rejection_returns_only_the_captured_draft_and_sessions_are_independent() {
        let first = key("runtime", "first");
        let other = key("runtime", "other");
        let switched = key("new-runtime", "first");
        let mut attempts = SendAttempts::default();
        assert!(attempts.begin(first.clone(), attempt(&first)));
        assert!(attempts.begin(other.clone(), attempt(&other)));
        assert!(attempts.begin(switched.clone(), attempt(&switched)));
        let rejected = attempts
            .finish(&first, Err(PromptAdmission::NotAttempted))
            .expect("definite failure restores");
        assert_eq!(rejected.owner.runtime(), &first.runtime);
        assert_eq!(
            rejected.owner.target(),
            &DraftTarget::Session(first.session.clone())
        );
        assert_eq!(
            rejected
                .prompt
                .model
                .as_ref()
                .and_then(|model| model.variant.as_deref()),
            Some("effort")
        );
        assert_eq!(rejected.prompt.agent.as_deref(), Some("agent"));
        assert!(attempts.can_send(&first));
        assert!(!attempts.can_send(&other));
        assert!(!attempts.can_send(&switched));
    }

    #[test]
    fn failed_text_appends_after_newer_text_without_changing_its_selection() {
        let newer = Draft {
            text: "newer".to_owned(),
            selection: 1..3,
        };
        let failed = Draft {
            text: "failed".to_owned(),
            selection: 2..5,
        };
        let restored = restore_failed(Some(&newer), failed.clone());
        assert_eq!(restored.text, "newer\n\nfailed");
        assert_eq!(restored.selection, newer.selection);
        assert_eq!(restore_failed(None, failed.clone()), failed);
        assert_eq!(restore_failed(Some(&failed), failed.clone()), failed);
    }

    #[test]
    fn rollback_deletes_only_its_optimistic_item() {
        let key = key("runtime", "session");
        let local = attempt(&key);
        let mut other = local.optimistic.clone();
        other.info.id = MessageId::from("another-message");
        let mut transcript = Transcript::new();
        let _ = transcript.push(local.optimistic.clone());
        let _ = transcript.push(other.clone());
        let _ = transcript.apply(TranscriptEvent::MessageRemoved {
            message_id: local.prompt.message_id,
        });
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript.message(0), Some(&other));
    }
}
