//! Window-lifetime mutation ownership, independent of current selection.

use std::collections::{HashMap, HashSet};

use openchamber_native_core::draft::{Draft, DraftKey};
use openchamber_native_core::ids::{MessageId, RuntimeId, SessionId};
use openchamber_native_core::transcript::{
    Message, Role, Transcript, TranscriptChange, TranscriptEvent,
};
use openchamber_native_transport::prompt::{Prompt, PromptAdmission};

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SendOrigin {
    #[default]
    Existing,
    Created,
    Deleted,
}

#[derive(Clone)]
pub(crate) struct SendAttempt {
    pub origin: SendOrigin,
    pub owner: DraftKey,
    pub draft: Draft,
    pub prompt: Prompt,
    pub optimistic: Message,
    pub admission: Admission,
}

/// Unresolved sends, several per session, in the order they began. At most
/// one of a session's attempts is not yet acknowledged (preparing, sending or
/// uncertain): a new identity waits while delivery is uncertain, so admission
/// order matches the order the user sent in. Acknowledged attempts stay until
/// an exact authoritative echo replaces their optimistic message.
///
/// `active` is the session's one send chain: a send or retry of a named
/// attempt. A second send chain waits, so selection, context and prompt
/// requests never interleave. `aborts` holds the sessions with an interrupt
/// in flight. Stop may start while a chain is still durable-saving or
/// rejecting the requests it supersedes, as the web client's Stop is never
/// gated; that chain then sends nothing. Once a chain's requests leave, Stop
/// waits for it, so an interrupt never runs beside selection or admission.
#[derive(Default)]
pub(crate) struct SendAttempts {
    attempts: HashMap<SessionKey, Vec<SendAttempt>>,
    active: HashMap<SessionKey, ActiveSend>,
    aborts: HashSet<SessionKey>,
}

#[derive(Clone, PartialEq, Eq)]
struct ActiveSend {
    id: MessageId,
    /// Its selection, context or prompt requests have started.
    dispatched: bool,
    /// Stop was pressed before they started. Lasts until the chain ends, so
    /// an interrupt that answers first cannot let them start after it.
    stopped: bool,
}

pub(crate) enum HistoryMerge {
    Latest,
    Earlier,
}

impl SendAttempt {
    fn id(&self) -> &MessageId {
        &self.prompt.message_id
    }

    fn acknowledged(&self) -> bool {
        self.admission == Admission::Admitted
    }
}

impl SendAttempts {
    /// Captures durable prompts only. Abort tasks have no replayable payload.
    pub(crate) fn snapshot(&self) -> Vec<(SessionKey, SendAttempt)> {
        self.iter()
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
            let session = restored.attempts.entry(key.clone()).or_default();
            if attempt.owner.runtime() != &key.runtime
                || attempt.owner.target() != &DraftTarget::Session(key.session.clone())
                || !identities.insert((key.runtime.clone(), attempt.id().clone()))
                || (!attempt.acknowledged() && session.iter().any(|held| !held.acknowledged()))
            {
                return Err("invalid or duplicate attempt owner or identity");
            }
            if matches!(
                attempt.admission,
                Admission::Preparing | Admission::Sending { .. }
            ) {
                attempt.admission = Admission::Uncertain;
            }
            session.push(attempt);
        }
        Ok(restored)
    }

    /// Whether a new identity may begin in this session: no chain runs and
    /// every retained attempt is acknowledged. A deleted created session keeps
    /// its saved text and never accepts another send.
    pub(crate) fn can_send(&self, key: &SessionKey) -> bool {
        !self.is_active(key)
            && self
                .session(key)
                .all(|attempt| attempt.acknowledged() && attempt.origin != SendOrigin::Deleted)
    }

    pub(crate) fn begin(&mut self, key: SessionKey, attempt: SendAttempt) -> bool {
        if !self.can_send(&key) || self.attempt(&key, attempt.id()).is_some() {
            return false;
        }
        self.start(key.clone(), attempt.id().clone());
        self.attempts.entry(key).or_default().push(attempt);
        true
    }

    fn start(&mut self, key: SessionKey, id: MessageId) {
        self.active.insert(
            key,
            ActiveSend {
                id,
                dispatched: false,
                stopped: false,
            },
        );
    }

    /// Starts an interrupt unless one runs or a send's requests have left.
    pub(crate) fn begin_abort(&mut self, key: SessionKey) -> bool {
        if self.aborts.contains(&key) {
            return false;
        }
        match self.active.get_mut(&key) {
            Some(send) if send.dispatched => return false,
            Some(send) => send.stopped = true,
            None => {}
        }
        self.aborts.insert(key)
    }

    /// Whether a send's requests are in flight, so Stop has to wait for them.
    pub(crate) fn abort_waits(&self, key: &SessionKey) -> bool {
        self.active.get(key).is_some_and(|send| send.dispatched) && !self.aborts.contains(key)
    }

    /// Marks this attempt's requests as leaving. Refused while an interrupt
    /// runs or once Stop has stopped this chain: the caller then treats the
    /// send as not attempted.
    pub(crate) fn dispatch(&mut self, key: &SessionKey, id: &MessageId) -> bool {
        if self.aborts.contains(key) {
            return false;
        }
        match self.active.get_mut(key) {
            Some(send) if send.id == *id && !send.stopped => {
                send.dispatched = true;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn is_active(&self, key: &SessionKey) -> bool {
        self.active.contains_key(key) || self.aborts.contains(key)
    }

    /// Whether this attempt's own send or retry chain is running.
    pub(crate) fn is_sending(&self, key: &SessionKey, id: &MessageId) -> bool {
        self.active.get(key).is_some_and(|send| send.id == *id)
    }

    pub(crate) fn prepared(&mut self, key: &SessionKey, id: &MessageId) -> bool {
        let Some(attempt) = self.attempt_mut(key, id) else {
            return false;
        };
        if attempt.origin == SendOrigin::Deleted || attempt.admission != Admission::Preparing {
            return false;
        }
        attempt.admission = Admission::Sending { observed: false };
        true
    }

    pub(crate) fn begin_retry(&mut self, key: &SessionKey, id: &MessageId) -> bool {
        if !self.attempt(key, id).is_some_and(|attempt| {
            attempt.origin != SendOrigin::Deleted && attempt.admission == Admission::Uncertain
        }) || self.is_active(key)
        {
            return false;
        }
        self.start(key.clone(), id.clone());
        true
    }

    pub(crate) fn confirm_pending(&mut self, key: &SessionKey, id: &MessageId) -> bool {
        let sending = self.is_sending(key, id);
        let Some(attempt) = self.attempt_mut(key, id) else {
            return false;
        };
        attempt.admission = if sending {
            Admission::Sending { observed: true }
        } else {
            Admission::Admitted
        };
        true
    }

    pub(crate) fn finish_abort(&mut self, key: &SessionKey) {
        self.aborts.remove(key);
    }

    /// Removes an uncertain attempt no chain or interrupt is using, so the
    /// caller can return its prompt to a draft. Acknowledged attempts are on
    /// the server and resolve only by their echo.
    pub(crate) fn discard(&mut self, key: &SessionKey, id: &MessageId) -> Option<SendAttempt> {
        if self.is_active(key)
            || !self.attempt(key, id).is_some_and(|attempt| {
                attempt.admission == Admission::Uncertain && attempt.origin != SendOrigin::Deleted
            })
        {
            return None;
        }
        self.remove(key, id)
    }

    pub(crate) fn attempt(&self, key: &SessionKey, id: &MessageId) -> Option<&SendAttempt> {
        self.session(key).find(|attempt| attempt.id() == id)
    }

    fn attempt_mut(&mut self, key: &SessionKey, id: &MessageId) -> Option<&mut SendAttempt> {
        self.attempts
            .get_mut(key)?
            .iter_mut()
            .find(|attempt| attempt.id() == id)
    }

    /// The session's attempts in the order they began.
    pub(crate) fn session(&self, key: &SessionKey) -> impl Iterator<Item = &SendAttempt> {
        self.attempts.get(key).into_iter().flatten()
    }

    /// The first send of a session this client created, while it is retained.
    pub(crate) fn created_attempt(&self, key: &SessionKey) -> Option<&SendAttempt> {
        self.session(key)
            .find(|attempt| attempt.origin != SendOrigin::Existing)
    }

    pub(crate) fn ids(&self, key: &SessionKey) -> Vec<MessageId> {
        self.session(key)
            .map(|attempt| attempt.id().clone())
            .collect()
    }

    pub(crate) fn mark_created_deleted(&mut self, key: &SessionKey) -> bool {
        let Some(attempt) = self.attempts.get_mut(key).and_then(|session| {
            session
                .iter_mut()
                .find(|attempt| attempt.origin == SendOrigin::Created)
        }) else {
            return false;
        };
        attempt.origin = SendOrigin::Deleted;
        true
    }

    /// Removes every attempt of a deleted session. A chain still running for
    /// it keeps its slot until it finishes, then finds nothing to update.
    pub(crate) fn forget_session(&mut self, key: &SessionKey) -> Vec<SendAttempt> {
        self.attempts.remove(key).unwrap_or_default()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&SessionKey, &SendAttempt)> {
        self.attempts
            .iter()
            .flat_map(|(key, session)| session.iter().map(move |attempt| (key, attempt)))
    }

    fn remove(&mut self, key: &SessionKey, id: &MessageId) -> Option<SendAttempt> {
        let session = self.attempts.get_mut(key)?;
        let index = session.iter().position(|attempt| attempt.id() == id)?;
        let removed = session.remove(index);
        if session.is_empty() {
            self.attempts.remove(key);
        }
        Some(removed)
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

    /// Replaces optimistic messages before core's part-preserving history
    /// merge. Missing IDs preserve unresolved optimistic content and admission.
    pub(crate) fn merge_history(
        &mut self,
        key: &SessionKey,
        transcript: &mut Transcript,
        messages: Vec<Message>,
        since: u64,
        merge: HistoryMerge,
    ) -> TranscriptChange {
        let incoming: Vec<Message> = self
            .session(key)
            .filter_map(|attempt| {
                messages.iter().find(|message| {
                    message.info.id == *attempt.id() && message.info.role == Role::User
                })
            })
            .cloned()
            .collect();
        let replacements: Vec<Message> = incoming
            .into_iter()
            .filter(|message| self.observe(key, &message.info.id))
            .collect();
        if !replacements.is_empty() && matches!(merge, HistoryMerge::Earlier) {
            // A retained optimistic row can sit after newer history. Place
            // its authoritative record in this page's order, while keeping
            // every other held row and core's newer-deletion protection.
            let replaced: HashSet<_> = replacements
                .iter()
                .map(|message| message.info.id.clone())
                .collect();
            let held: HashSet<_> = transcript
                .messages()
                .iter()
                .map(|message| message.info.id.clone())
                .collect();
            let mut combined: Vec<_> = messages
                .into_iter()
                .filter(|message| {
                    replaced.contains(&message.info.id) || !held.contains(&message.info.id)
                })
                .collect();
            combined.extend(
                transcript
                    .messages()
                    .iter()
                    .filter(|message| !replaced.contains(&message.info.id))
                    .cloned(),
            );
            let changed = transcript.materialize(combined, since) != TranscriptChange::Unchanged;
            let mut pushed = false;
            for authoritative in replacements {
                pushed |= transcript.row_of(&authoritative.info.id).is_some()
                    && transcript.push(authoritative) != TranscriptChange::Unchanged;
            }
            return if changed || pushed {
                TranscriptChange::Reset
            } else {
                TranscriptChange::Unchanged
            };
        }
        let mut replaced = false;
        for message in replacements {
            replaced |= transcript.push(message) != TranscriptChange::Unchanged;
        }
        let change = match merge {
            HistoryMerge::Latest => transcript.materialize(messages, since),
            HistoryMerge::Earlier => transcript.prepend_page(messages, since),
        };
        let mut restored = false;
        for attempt in self.attempts.get(key).into_iter().flatten() {
            if matches!(
                attempt.admission,
                Admission::Preparing | Admission::Sending { observed: true }
            ) || transcript.row_of(attempt.id()).is_some()
            {
                continue;
            }
            restored |= transcript.push(attempt.optimistic.clone()) != TranscriptChange::Unchanged;
        }
        if replaced || restored {
            TranscriptChange::Reset
        } else {
            change
        }
    }

    /// Only exact authoritative identity proves admission. The first such
    /// record replaces local content, including plugin-transformed text.
    pub(crate) fn observe(&mut self, key: &SessionKey, id: &MessageId) -> bool {
        let sending = self.is_sending(key, id);
        let Some(attempt) = self.attempt_mut(key, id) else {
            return false;
        };
        if attempt.origin == SendOrigin::Deleted {
            return false;
        }
        match attempt.admission {
            Admission::Preparing => {
                self.remove(key, id);
                if sending {
                    self.active.remove(key);
                }
                true
            }
            Admission::Sending { observed: true } => false,
            Admission::Sending { observed: false } => {
                attempt.admission = Admission::Sending { observed: true };
                true
            }
            Admission::Admitted | Admission::Uncertain => {
                if sending {
                    attempt.admission = Admission::Sending { observed: true };
                } else {
                    self.remove(key, id);
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
        id: &MessageId,
        outcome: Result<(), PromptAdmission>,
    ) -> Option<SendAttempt> {
        if self.is_sending(key, id) {
            self.active.remove(key);
        }
        let attempt = self.attempt_mut(key, id)?;
        if attempt.origin == SendOrigin::Deleted {
            return None;
        }
        if attempt.admission == (Admission::Sending { observed: true }) {
            self.remove(key, id);
            return None;
        }
        if attempt.admission == Admission::Uncertain && outcome.is_err() {
            return None;
        }
        match outcome {
            Ok(()) => attempt.admission = Admission::Admitted,
            Err(PromptAdmission::Unknown | PromptAdmission::Partial) => {
                attempt.admission = Admission::Uncertain;
            }
            Err(PromptAdmission::NotAttempted | PromptAdmission::Rejected) => {
                return self.remove(key, id);
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

    #[test]
    fn deleted_created_session_keeps_text_and_rejects_late_completion_and_retry() {
        let key = key("runtime", "ses_deleted");
        let mut attempt = attempt(&key);
        attempt.origin = SendOrigin::Created;
        let id = attempt.prompt.message_id.clone();
        let mut attempts = SendAttempts::default();
        assert!(attempts.begin(key.clone(), attempt));
        assert!(attempts.mark_created_deleted(&key));
        assert!(!attempts.observe(&key, &id));
        assert!(attempts.finish(&key, &sent_id(), Ok(())).is_none());
        assert!(!attempts.begin_retry(&key, &sent_id()));
        assert!(!attempts.can_send(&key));
        assert_eq!(
            attempts.attempt(&key, &sent_id()).expect("retained").origin,
            SendOrigin::Deleted
        );
        let restored = SendAttempts::from_snapshot(attempts.snapshot()).expect("restore");
        assert_eq!(
            restored.attempt(&key, &sent_id()).expect("retained").origin,
            SendOrigin::Deleted
        );
        assert_eq!(
            restored
                .attempt(&key, &sent_id())
                .expect("saved text")
                .draft
                .text,
            "submitted text"
        );
    }

    pub(crate) fn key(runtime: &str, session: &str) -> SessionKey {
        SessionKey {
            runtime: RuntimeId::from(runtime),
            session: SessionId::from(session),
        }
    }

    /// The message ID every [`attempt`] carries.
    pub(crate) fn sent_id() -> MessageId {
        MessageId::from("msg_bcfe56800001AbCdEf01234567")
    }

    pub(crate) fn attempt(key: &SessionKey) -> SendAttempt {
        let draft = Draft {
            text: "submitted text".to_owned(),
            selection: 2..6,
        };
        let message_id = sent_id();
        SendAttempt {
            origin: SendOrigin::Existing,
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
            prompt: Prompt {
                message_id,
                text: draft.text,
                parts: openchamber_native_transport::prompt_parts::PromptParts::default(),
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
                attempts.finish(&key, &sent_id(), Ok(()));
            }
            let _ = attempts.merge_history(
                &key,
                &mut transcript,
                vec![authoritative.clone()],
                0,
                HistoryMerge::Latest,
            );
            if event_first {
                attempts.finish(&key, &sent_id(), Ok(()));
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
                attempts.finish(&key, &sent_id(), Ok(()));
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
                attempts.finish(&key, &sent_id(), Ok(()));
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
        attempts.finish(&key, &sent_id(), Err(PromptAdmission::Unknown));
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
        assert!(attempts.dispatch(&key, &id), "its requests leave");
        assert!(attempts.observe(&key, &id));
        assert!(
            !attempts.can_send(&key),
            "the mutation chain still owns its slot"
        );
        assert!(!attempts.begin_abort(key.clone()));
        assert!(
            attempts
                .finish(&key, &sent_id(), Err(PromptAdmission::Rejected))
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
        attempts.finish(&key, &sent_id(), Err(PromptAdmission::Unknown));
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
            .attempt(&key, &sent_id())
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
            recovered
                .attempt(&key, &sent_id())
                .expect("retained")
                .admission,
            Admission::Uncertain
        );
        assert!(attempts.prepared(&key, &id));
        assert_eq!(
            attempts
                .attempt(&key, &sent_id())
                .expect("retained")
                .admission,
            Admission::Sending { observed: false }
        );
    }

    #[test]
    fn a_failed_retry_cannot_reject_the_original_uncertain_send() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        attempts.begin(key.clone(), attempt(&key));
        attempts.finish(&key, &sent_id(), Err(PromptAdmission::Unknown));
        let id = attempts
            .attempt(&key, &sent_id())
            .expect("retained")
            .prompt
            .message_id
            .clone();
        assert!(attempts.begin_retry(&key, &sent_id()));
        assert!(!attempts.begin_retry(&key, &sent_id()));
        assert!(
            attempts
                .finish(&key, &sent_id(), Err(PromptAdmission::Rejected))
                .is_none()
        );
        assert_eq!(
            attempts
                .attempt(&key, &sent_id())
                .expect("retained")
                .admission,
            Admission::Uncertain
        );
        assert_eq!(
            attempts
                .attempt(&key, &sent_id())
                .expect("retained")
                .prompt
                .message_id,
            id
        );
    }

    #[test]
    fn observing_an_active_retry_keeps_ownership_until_its_response() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        attempts.begin(key.clone(), attempt(&key));
        attempts.finish(&key, &sent_id(), Err(PromptAdmission::Unknown));
        let id = attempts
            .attempt(&key, &sent_id())
            .expect("retained")
            .prompt
            .message_id
            .clone();
        assert!(attempts.begin_retry(&key, &sent_id()));
        assert!(attempts.observe(&key, &id));
        assert!(
            attempts.attempt(&key, &sent_id()).is_some(),
            "the response still owns the attempt"
        );
        assert!(!attempts.can_send(&key));
        assert!(
            attempts
                .finish(&key, &sent_id(), Err(PromptAdmission::Unknown))
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
        attempts.finish(&key, &sent_id(), Err(PromptAdmission::Unknown));
        let id = attempts
            .attempt(&key, &sent_id())
            .expect("retained")
            .prompt
            .message_id
            .clone();
        assert!(attempts.begin_abort(key.clone()));
        assert!(attempts.observe(&key, &id));
        assert!(
            attempts.attempt(&key, &sent_id()).is_none(),
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
            .finish(&first, &sent_id(), Err(PromptAdmission::NotAttempted))
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

    fn second(key: &SessionKey, id: &str) -> SendAttempt {
        let mut attempt = attempt(key);
        attempt.prompt.message_id = MessageId::from(id);
        attempt.optimistic.info.id = MessageId::from(id);
        attempt.prompt.delivery = Some(openchamber_native_transport::prompt::Delivery::Steer);
        attempt
    }

    #[test]
    fn acknowledged_attempts_admit_another_send_and_resolve_independently() {
        let key = key("runtime", "session");
        let steer = MessageId::from("msg_bcfe56800002Steer");
        let mut attempts = SendAttempts::default();
        assert!(attempts.begin(key.clone(), attempt(&key)));
        assert!(
            !attempts.begin(key.clone(), second(&key, steer.as_str())),
            "the first chain is still running"
        );
        assert!(attempts.finish(&key, &sent_id(), Ok(())).is_none());
        assert!(
            attempts.can_send(&key),
            "an acknowledged send admits a steer"
        );
        assert!(attempts.begin(key.clone(), second(&key, steer.as_str())));
        assert!(attempts.dispatch(&key, &steer));
        assert!(attempts.is_sending(&key, &steer));
        assert!(!attempts.is_sending(&key, &sent_id()));
        assert!(
            !attempts.begin_abort(key.clone()),
            "abort waits for the steer chain"
        );
        assert!(
            attempts.observe(&key, &sent_id()),
            "the first echo resolves only it"
        );
        assert!(attempts.attempt(&key, &sent_id()).is_none());
        assert!(attempts.attempt(&key, &steer).is_some());
        assert!(attempts.finish(&key, &steer, Ok(())).is_none());
        assert!(
            attempts.begin_abort(key.clone()),
            "abort runs beside acknowledged sends"
        );
        assert!(!attempts.can_send(&key), "a send waits for the abort");
        attempts.finish_abort(&key);
        assert!(attempts.can_send(&key));
        assert_eq!(attempts.ids(&key), std::slice::from_ref(&steer));
        let restored = SendAttempts::from_snapshot(attempts.snapshot()).expect("restore");
        assert_eq!(
            restored.attempt(&key, &steer).expect("kept").admission,
            Admission::Admitted
        );
    }

    #[test]
    fn an_uncertain_attempt_blocks_new_identities_and_snapshots_hold_one() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        assert!(attempts.begin(key.clone(), attempt(&key)));
        attempts.finish(&key, &sent_id(), Err(PromptAdmission::Unknown));
        assert!(!attempts.can_send(&key));
        assert!(!attempts.begin(key.clone(), second(&key, "msg_bcfe56800002Steer")));
        let mut snapshot = attempts.snapshot();
        snapshot.push((key.clone(), second(&key, "msg_bcfe56800003Other")));
        assert!(
            SendAttempts::from_snapshot(snapshot).is_err(),
            "two unresolved attempts in one session cannot be ordered"
        );
        let mut duplicate = attempts.snapshot();
        let mut acknowledged = attempt(&key);
        acknowledged.admission = Admission::Admitted;
        duplicate.push((key.clone(), acknowledged));
        assert!(
            SendAttempts::from_snapshot(duplicate).is_err(),
            "one ID never names two attempts"
        );
    }

    #[test]
    fn history_replaces_every_echoed_attempt_and_restores_the_rest() {
        let key = key("runtime", "session");
        let steer = MessageId::from("msg_bcfe56800002Steer");
        let mut attempts = SendAttempts::default();
        let first = attempt(&key);
        let later = second(&key, steer.as_str());
        let mut transcript = Transcript::new();
        assert!(attempts.begin(key.clone(), first.clone()));
        attempts.finish(&key, &sent_id(), Ok(()));
        assert!(attempts.begin(key.clone(), later.clone()));
        attempts.finish(&key, &steer, Ok(()));
        let mut echoed = first.optimistic.clone();
        echoed.parts = vec![Part::text(PartId::from("server-part"), "prepared")];
        let _ = attempts.merge_history(
            &key,
            &mut transcript,
            vec![echoed.clone()],
            0,
            HistoryMerge::Latest,
        );
        assert_eq!(transcript.messages(), &[echoed, later.optimistic.clone()]);
        assert_eq!(attempts.ids(&key), [steer]);
    }

    #[test]
    fn a_partial_refusal_keeps_the_attempt_for_recovery() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        assert!(attempts.begin(key.clone(), attempt(&key)));
        assert!(
            attempts
                .finish(&key, &sent_id(), Err(PromptAdmission::Partial))
                .is_none(),
            "admitted context cannot be rolled back to a draft"
        );
        assert_eq!(
            attempts.attempt(&key, &sent_id()).expect("kept").admission,
            Admission::Uncertain
        );
        assert!(!attempts.can_send(&key));
    }

    #[test]
    fn stop_runs_during_request_rejection_and_keeps_the_prompt_unsent() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        let mut local = attempt(&key);
        local.admission = Admission::Preparing;
        assert!(attempts.begin(key.clone(), local));
        assert!(
            attempts.begin_abort(key.clone()),
            "nothing of the send has reached the session yet"
        );
        assert!(
            !attempts.begin_abort(key.clone()),
            "one interrupt at a time"
        );
        assert!(
            !attempts.dispatch(&key, &sent_id()),
            "no selection or prompt request runs beside an interrupt"
        );
        assert!(
            attempts
                .finish(&key, &sent_id(), Err(PromptAdmission::NotAttempted))
                .is_some(),
            "the stopped send returns to its draft"
        );
        assert!(!attempts.can_send(&key), "the interrupt still runs");
        attempts.finish_abort(&key);
        assert!(attempts.can_send(&key));

        let mut local = attempt(&key);
        local.admission = Admission::Preparing;
        assert!(attempts.begin(key.clone(), local));
        assert!(!attempts.abort_waits(&key), "Stop can still run now");
        assert!(attempts.dispatch(&key, &sent_id()));
        assert!(attempts.abort_waits(&key), "Stop tells the user it waits");
        assert!(
            !attempts.begin_abort(key.clone()),
            "once its requests leave, the send finishes before an interrupt"
        );
    }

    #[test]
    fn stop_holds_when_the_interrupt_answers_before_dispatch() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        let mut local = attempt(&key);
        local.admission = Admission::Preparing;
        assert!(attempts.begin(key.clone(), local));
        assert!(attempts.begin_abort(key.clone()));
        attempts.finish_abort(&key);
        assert!(
            !attempts.dispatch(&key, &sent_id()),
            "a send stopped before its requests left never sends them"
        );
        assert!(
            attempts
                .finish(&key, &sent_id(), Err(PromptAdmission::NotAttempted))
                .is_some()
        );
        let mut local = attempt(&key);
        local.admission = Admission::Preparing;
        assert!(attempts.begin(key.clone(), local));
        assert!(
            attempts.dispatch(&key, &sent_id()),
            "the next send is not stopped"
        );
    }

    #[test]
    fn only_an_idle_uncertain_attempt_can_be_discarded() {
        let key = key("runtime", "session");
        let mut attempts = SendAttempts::default();
        assert!(attempts.begin(key.clone(), attempt(&key)));
        assert!(
            attempts.discard(&key, &sent_id()).is_none(),
            "its chain runs"
        );
        attempts.finish(&key, &sent_id(), Err(PromptAdmission::Unknown));
        assert!(attempts.begin_retry(&key, &sent_id()));
        assert!(attempts.discard(&key, &sent_id()).is_none(), "a retry runs");
        attempts.finish(&key, &sent_id(), Err(PromptAdmission::Rejected));
        let discarded = attempts
            .discard(&key, &sent_id())
            .expect("idle and uncertain");
        assert_eq!(discarded.draft.text, "submitted text");
        assert!(attempts.can_send(&key));
        assert!(attempts.snapshot().is_empty(), "nothing is left to journal");
        let mut acknowledged = attempt(&key);
        acknowledged.admission = Admission::Admitted;
        let mut attempts =
            SendAttempts::from_snapshot(vec![(key.clone(), acknowledged)]).expect("restore");
        assert!(
            attempts.discard(&key, &sent_id()).is_none(),
            "an acknowledged prompt is on the server and resolves by its echo"
        );
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
