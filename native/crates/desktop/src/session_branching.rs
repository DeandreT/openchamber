//! Fork, staged revert, `/btw` and subagent navigation from the chat.
//!
//! Follows `session-actions.ts` (`forkFromMessage`, `forkFromLastCompletedTurn`,
//! `revertToMessage`, `clearStagedRevert`), `btw.ts` and `subagent-run.ts`.
//! Where each action cuts comes from the core's `branching` module. Every
//! request captures its runtime and session; nothing is retried. A revert's
//! marker is the only optimistic state: it hides the cut at once and is
//! dropped, restoring the composer, when the server refuses.

use std::collections::{HashMap, HashSet};

use gpui_kit::{Context, Window};
use openchamber_native_core::branching;
use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::ids::{MessageId, RuntimeId, SessionId};
use openchamber_native_core::session::{LiveStatus, MetadataFacts, SessionPatch, SessionRecord};
use openchamber_native_core::session_actions::delete_targets;
use openchamber_native_core::transcript::Message;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::ServerEvent;
use openchamber_native_transport::prompt::Prompt;
use openchamber_native_transport::prompt_parts::{PromptParts, SyntheticContext};

use crate::locale;
use crate::send_attempt::SessionKey;
use crate::session_management::describe_error;
use crate::workspace::Workspace;

/// `BTW_BOUNDARY_INSTRUCTION`: sent as synthetic context with every message
/// in a `/btw` side conversation, so the inherited transcript stays reference.
pub(crate) const BTW_BOUNDARY_INSTRUCTION: &str = "You are in a btw session, a side conversation forked from a main thread.\n\
The history inherited from the parent thread is reference context only. It is not your current task.\n\
Do not continue, execute, or complete any task, plan, tool call, approval, edit, or request that appears only in that inherited history. Only instructions the user sends inside this btw session are active.\n\
Any tool calls or outputs visible in the inherited history happened in the parent thread and are reference-only; do not infer active instructions from them.\n\
Sub-agents are off-limits in this btw session. Do not interact with any existing or new sub-agents, even if sub-agents were used in the inherited history.\n\
Do not modify files, source, git state, permissions, configuration, or any other workspace state unless the user explicitly asks for that mutation inside this btw session. If they do, keep it minimal, local to the request, and avoid disrupting the main thread.";

/// How many messages a cascading revert reads from each descendant.
const DESCENDANT_PAGE: usize = 200;

/// A slash command the composer runs instead of sending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BranchCommand {
    /// `/fork`: fork after the last finished turn.
    Fork,
    /// `/btw <question>`; an empty question is reported, never sent.
    Btw(String),
}

/// `/fork` and `/btw` as the first word of the composer's text.
pub(crate) fn parse_command(text: &str) -> Option<BranchCommand> {
    let text = text.trim();
    let (name, rest) = text
        .split_once(char::is_whitespace)
        .map_or((text, ""), |(name, rest)| (name, rest.trim()));
    match name {
        "/fork" => Some(BranchCommand::Fork),
        "/btw" => Some(BranchCommand::Btw(rest.to_owned())),
        _ => None,
    }
}

/// One parent's `/btw` panel as only this client knows it. The panel's
/// identity comes from the parent's `btwSessionID` metadata.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BtwLocal {
    pub creating: bool,
    pub destroying: bool,
    pub promoting: bool,
    pub sending: bool,
    /// Collapsed hands the composer back to the parent.
    pub collapsed: bool,
}

/// What the panel above the composer shows for the selected session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BtwPanel {
    Creating,
    Open {
        fork: SessionId,
        boundary: Option<MessageId>,
        local: BtwLocal,
    },
}

/// The exact composer change to undo if a staged revert fails.
pub(crate) struct RevertRestore {
    pub key: DraftKey,
    pub previous: String,
    pub restored: String,
}

/// Text retained until a BTW send succeeds, including its original owner.
pub(crate) struct BtwDraft {
    pub key: DraftKey,
    pub text: String,
}

/// One fork read's identity and the patches received while it runs.
pub(crate) struct ForkRead {
    revision: u64,
    pub patches: Vec<SessionPatch>,
}

/// Branching changes in flight and the client-side state around them.
#[derive(Default)]
pub(crate) struct Branching {
    /// Optimistic revert markers until the server answers the stage.
    pub(crate) pending_reverts: HashMap<SessionKey, MessageId>,
    pub(crate) restoring: HashSet<SessionKey>,
    pub(crate) forking: HashSet<SessionKey>,
    pub(crate) fork_reads: HashMap<SessionKey, ForkRead>,
    next_fork_read: u64,
    /// The strip's list is open for one marker of one session.
    pub(crate) strip_expanded: Option<(SessionKey, MessageId)>,
    pub(crate) btw: HashMap<SessionKey, BtwLocal>,
    /// Child sessions whose stop is in flight.
    pub(crate) stopping: HashSet<SessionKey>,
}

impl Branching {
    /// Forgets client-side overlays for a runtime switch. Requests in flight
    /// finish against their captured runtime and edit nothing here.
    pub(crate) fn switch_runtime(&mut self) {
        *self = Self {
            next_fork_read: self.next_fork_read,
            ..Self::default()
        };
    }

    pub(crate) fn begin_fork_read(&mut self, key: SessionKey) -> u64 {
        self.next_fork_read += 1;
        let revision = self.next_fork_read;
        self.fork_reads.insert(
            key,
            ForkRead {
                revision,
                patches: Vec::new(),
            },
        );
        revision
    }
}

impl Workspace {
    /// The marker that hides the selected transcript's tail: an optimistic
    /// one first, else the server's.
    pub(crate) fn revert_marker(&self, session: &SessionId) -> Option<MessageId> {
        let key = self.mutation_key(session);
        self.branching
            .pending_reverts
            .get(&key)
            .cloned()
            .or_else(|| self.sessions.get(session)?.revert.clone())
    }

    /// How many leading rows of a visited transcript are shown.
    pub(crate) fn visible_rows(&self, session: &SessionId) -> usize {
        let Some(open) = self.open_sessions.get(session) else {
            return 0;
        };
        branching::visible_len(
            open.transcript.messages(),
            self.revert_marker(session).as_ref(),
        )
    }

    /// Rows change height when the cut moves.
    pub(crate) fn remeasure_transcript(&self, session: &SessionId) {
        if let Some(open) = self.open_sessions.get(session) {
            let count = open.list.item_count();
            if count > 0 {
                open.list.remeasure_items(0..count);
            }
        }
    }

    fn session_message(&self, session: &SessionId, id: &MessageId) -> Option<&Message> {
        let open = self.open_sessions.get(session)?;
        open.transcript
            .row_of(id)
            .and_then(|row| open.transcript.message(row))
    }

    fn branch_draft_key(&self, session: &SessionId) -> Option<DraftKey> {
        let directory = self.sessions.get(session)?.directory.clone()?;
        DraftKey::new(
            self.runtime.clone(),
            &directory,
            DraftTarget::Session(session.clone()),
        )
    }

    /// Puts `text` in a session's composer, replacing what it held.
    fn put_in_composer(
        &mut self,
        key: DraftKey,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_composer_draft(cx);
        self.drafts.save(key.clone(), Draft::new(text));
        if self.composer_key.as_ref() == Some(&key) {
            self.restore_composer_draft(window, cx);
        }
        self.queue_persistence(cx);
    }

    fn draft_text(&mut self, key: &DraftKey, cx: &mut Context<Self>) -> String {
        if self.composer_key.as_ref() == Some(key) {
            self.save_composer_draft(cx);
        }
        self.drafts
            .get(key)
            .map(|draft| draft.text.clone())
            .unwrap_or_default()
    }

    fn capture_btw_draft(&mut self, cx: &mut Context<Self>) -> Option<BtwDraft> {
        let key = self.composer_key.clone()?;
        let text = self.draft_text(&key, cx);
        self.queue_persistence(cx);
        Some(BtwDraft { key, text })
    }

    pub(crate) fn clear_sent_btw_draft(
        &mut self,
        draft: BtwDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.draft_text(&draft.key, cx) == draft.text {
            self.put_in_composer(draft.key, String::new(), window, cx);
        }
    }

    fn branching_error(
        &mut self,
        title: &str,
        error: Option<&TransportError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let detail = error.map(|error| describe_error(locale::strings(cx), error));
        self.push_error(title, detail, window, cx);
    }

    // --- Fork ---------------------------------------------------------------

    /// Forks the selected session before one of its prompts and opens the
    /// fork with that prompt's text in its composer (`forkFromMessage`).
    pub(crate) fn fork_from_message(
        &mut self,
        message: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let Some(target) = self.session_message(&session, &message) else {
            return;
        };
        let restore = branching::restored_prompt_text(target);
        let open = &self.open_sessions[&session];
        let cut = branching::transcript_cut(open.transcript.messages(), &message);
        self.start_fork(session, Some(cut), restore, window, cx);
    }

    /// Forks the selected session after one of its messages, keeping that
    /// message and everything before it (`forkAfterMessage`).
    pub(crate) fn fork_after_message(
        &mut self,
        message: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let messages = self
            .open_sessions
            .get(&session)
            .map(|open| open.transcript.messages())
            .unwrap_or_default();
        let Some(cut) = branching::fork_after_cut(messages, &message) else {
            self.branching_error("chat.chatInput.toast.forkNothingToFork", None, window, cx);
            return;
        };
        self.start_fork(session, cut, None, window, cx);
    }

    /// `/fork`: forks after the last finished turn. A running turn is left
    /// out (`forkFromLastCompletedTurn`).
    pub(crate) fn fork_after_last_turn(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let running = self.is_streaming(&session);
        let messages = self
            .open_sessions
            .get(&session)
            .map(|open| open.transcript.messages())
            .unwrap_or_default();
        let Some(cut) = branching::last_completed_turn(messages, running)
            .and_then(|id| branching::fork_after_cut(messages, &id))
        else {
            self.branching_error("chat.chatInput.toast.forkNothingToFork", None, window, cx);
            return;
        };
        self.start_fork(session, cut, None, window, cx);
    }

    fn start_fork(
        &mut self,
        session: SessionId,
        before: Option<MessageId>,
        restore: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.mutation_key(&session);
        if self.branching.forking.contains(&key) {
            return;
        }
        let navigation = self.navigation_revision;
        let finish_key = key.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    let mut fork = client.fork_session(&session, before.as_ref()).await?;
                    // Best effort, as `applyForkInheritance`: the fork works
                    // even when its copied links could not be dropped.
                    match client.clean_fork_links(&fork.id).await {
                        Ok(Some(facts)) => {
                            fork.overlay = facts.overlay;
                            fork.kind = facts.kind;
                            fork.btw = facts.btw;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(%error, "the fork kept its source's links");
                        }
                    }
                    Ok(fork)
                })
            },
            move |this, result, window, cx| {
                this.finish_fork(finish_key, navigation, restore, result, window, cx)
            },
            window,
            cx,
        );
        if started {
            self.branching.forking.insert(key);
            cx.notify();
        }
    }

    pub(crate) fn finish_fork(
        &mut self,
        key: SessionKey,
        navigation: u64,
        restore: Option<String>,
        result: Option<Result<SessionRecord, TransportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.branching.forking.remove(&key);
        if key.runtime != self.runtime {
            return;
        }
        match result {
            Some(Ok(fork)) => {
                let id = fork.id.clone();
                self.apply_server_events(vec![ServerEvent::SessionCreated(fork)], window, cx);
                if let (Some(text), Some(draft)) = (restore, self.branch_draft_key(&id)) {
                    self.put_in_composer(draft, text, window, cx);
                }
                // Opens only while the reader is still where the fork began.
                if self.navigation_revision == navigation {
                    self.select_session(id, window, cx);
                }
            }
            None => {}
            Some(Err(error)) => {
                self.branching_error("chat.chatInput.toast.forkFailed", Some(&error), window, cx)
            }
        }
        cx.notify();
    }

    /// A fork this client did not start: OpenCode sends no record with
    /// `session.forked`, so it is read. A `/btw` start marks its fork only
    /// after it exists and inserts it itself (`forked-session.ts`).
    pub(crate) fn on_session_forked(
        &mut self,
        session: SessionId,
        parent: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let starting_btw = self
            .branching
            .btw
            .get(&self.mutation_key(parent))
            .is_some_and(|local| local.creating);
        let key = self.mutation_key(&session);
        if starting_btw
            || self.branching.forking.contains(&self.mutation_key(parent))
            || self.sessions.get(&session).is_some()
            || self.branching.fork_reads.contains_key(&key)
        {
            return;
        }
        let revision = self.branching.begin_fork_read(key.clone());
        self.read_fork(key, revision, false, window, cx);
    }

    fn read_fork(
        &mut self,
        key: SessionKey,
        revision: u64,
        reread: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = key.session.clone();
        let finish_key = key.clone();
        let started = self.spawn_session_task(
            move |client| Box::pin(async move { client.session(&session).await }),
            move |this, result, window, cx| {
                this.finish_fork_read(finish_key, revision, reread, result, window, cx);
            },
            window,
            cx,
        );
        if !started {
            self.branching.fork_reads.remove(&key);
        }
    }

    pub(crate) fn finish_fork_read(
        &mut self,
        key: SessionKey,
        revision: u64,
        reread: bool,
        result: Option<Result<SessionRecord, TransportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if key.runtime != self.runtime
            || !self
                .branching
                .fork_reads
                .get(&key)
                .is_some_and(|read| read.revision == revision)
        {
            return;
        }
        let read = self
            .branching
            .fork_reads
            .remove(&key)
            .expect("the matching read is present");
        let patches = read.patches;
        if self.sessions.get(&key.session).is_some() {
            return;
        }
        let Some(Ok(mut record)) = result else {
            return;
        };
        if !patches.is_empty() && !reread {
            self.branching.fork_reads.insert(
                key.clone(),
                ForkRead {
                    revision,
                    patches: Vec::new(),
                },
            );
            self.read_fork(key, revision, true, window, cx);
            return;
        }
        // Patches during the bounded follow-up read are newer than its
        // snapshot. Apply them in arrival order without starting another read.
        for patch in patches {
            record = record.patched(&patch);
        }
        self.apply_server_events(vec![ServerEvent::SessionCreated(record)], window, cx);
    }

    // --- Staged revert ------------------------------------------------------

    /// Reverts the selected session to before one of its prompts or subagent
    /// reports (`revertToMessage`): a running turn stops first, the cut hides
    /// at once, descendants revert first and the prompt's text returns to the
    /// composer. A refusal drops the marker and puts the composer back.
    pub(crate) fn revert_to_message(
        &mut self,
        message: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&session);
        if self.branching.pending_reverts.contains_key(&key)
            || self.branching.restoring.contains(&key)
            || !self.session_actions_available()
        {
            return;
        }
        let Some(target) = self.session_message(&session, &message).cloned() else {
            return;
        };
        let messages = self.open_sessions[&session].transcript.messages();
        let cut = branching::transcript_cut(messages, &message);
        let restore = branching::restored_prompt_text(&target);
        let busy = self.is_streaming(&session);
        let child_created = branching::target_child(&target)
            .and_then(|child| self.sessions.get(child))
            .map(|child| child.time.created);
        let cutoff = branching::descendant_cutoff(&target, child_created);
        let descendants: Vec<(SessionId, bool)> = delete_targets(&self.sessions, &session)
            .into_iter()
            .skip(1)
            .map(|id| {
                let busy = matches!(
                    self.live_status.get(&id),
                    Some(LiveStatus::Busy | LiveStatus::Retry)
                );
                (id, busy)
            })
            .collect();

        let draft = self.branch_draft_key(&session);
        let rollback = draft
            .as_ref()
            .zip(restore.as_ref())
            .map(|(draft, restored)| RevertRestore {
                key: draft.clone(),
                previous: self.draft_text(draft, cx),
                restored: restored.clone(),
            });
        let id = session.clone();
        let stage_cut = cut.clone();
        let finish_key = key.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    // A running turn would write past the cut.
                    if busy {
                        let _ = client.abort_session(&id).await;
                    }
                    // Descendants first: OpenCode restores file snapshots, and
                    // the parent's must win.
                    for (child, busy) in descendants {
                        if busy {
                            let _ = client.abort_session(&child).await;
                        }
                        let target = match client.latest_messages(&child, DESCENDANT_PAGE).await {
                            Ok(page) => {
                                branching::first_user_at_or_after(&page.messages, cutoff).cloned()
                            }
                            Err(error) => {
                                tracing::warn!(%error, "a descendant's revert target could not be read");
                                None
                            }
                        };
                        if let Some(target) = target
                            && let Err(error) = client.stage_revert(&child, &target).await
                        {
                            tracing::warn!(%error, "a descendant was not reverted");
                        }
                    }
                    client.stage_revert(&id, &stage_cut).await
                })
            },
            move |this, result, window, cx| {
                this.finish_revert(finish_key, rollback, result, window, cx)
            },
            window,
            cx,
        );
        if !started {
            return;
        }
        self.branching.pending_reverts.insert(key, cut);
        self.branching.strip_expanded = None;
        self.remeasure_transcript(&session);
        if let (Some(text), Some(draft)) = (restore, draft) {
            self.show_reverted_prompt(draft, text, window, cx);
        }
        cx.notify();
    }

    /// The reverted prompt goes back to its composer, which takes the focus
    /// when it is the one on screen, as the reference's does.
    pub(crate) fn show_reverted_prompt(
        &mut self,
        draft: DraftKey,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self.composer_key.as_ref() == Some(&draft);
        self.put_in_composer(draft, text, window, cx);
        if shown {
            self.focus_composer(window, cx);
        }
    }

    pub(crate) fn finish_revert(
        &mut self,
        key: SessionKey,
        rollback: Option<RevertRestore>,
        result: Option<Result<MessageId, TransportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.branching.pending_reverts.remove(&key);
        if matches!(result, Some(Err(_)))
            && let Some(rollback) = rollback
        {
            self.rollback_revert(rollback, window, cx);
        }
        if key.runtime != self.runtime {
            return;
        }
        match result {
            Some(Ok(marker)) => {
                let event = ServerEvent::SessionPatched {
                    session_id: key.session.clone(),
                    patch: SessionPatch {
                        revert: Some(Some(marker)),
                        ..SessionPatch::default()
                    },
                };
                self.apply_server_events(vec![event], window, cx);
                if let Some(directory) = self
                    .sessions
                    .get(&key.session)
                    .and_then(SessionRecord::resolved_directory)
                {
                    // The revert rolled the files back.
                    self.refresh_git_status_soon(&directory, cx);
                }
            }
            Some(Err(error)) => {
                self.branching_error("chat.messageBody.actions.revert", Some(&error), window, cx);
            }
            // A closed window or a cancelled request: the next read or event
            // settles the marker.
            None => {}
        }
        self.remeasure_transcript(&key.session);
        cx.notify();
    }

    /// Drops the optimistic marker and returns the composer to what it held,
    /// unless the user typed over the restored prompt meanwhile.
    fn rollback_revert(
        &mut self,
        rollback: RevertRestore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.draft_text(&rollback.key, cx) == rollback.restored {
            self.put_in_composer(rollback.key, rollback.previous, window, cx);
        }
    }

    /// Restore: clears the staged revert in descendants, then the session
    /// (`clearStagedRevert`). The marker goes when the server confirms.
    pub(crate) fn restore_revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&session);
        if self.branching.restoring.contains(&key)
            || self.branching.pending_reverts.contains_key(&key)
        {
            return;
        }
        let descendants: Vec<SessionId> = delete_targets(&self.sessions, &session)
            .into_iter()
            .skip(1)
            .filter(|id| {
                self.sessions
                    .get(id)
                    .is_some_and(|record| record.revert.is_some())
            })
            .collect();
        let id = session.clone();
        let finish_key = key.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    for child in descendants {
                        if let Err(error) = client.clear_revert(&child).await {
                            tracing::warn!(%error, "a descendant kept its staged revert");
                        }
                    }
                    client.clear_revert(&id).await
                })
            },
            move |this, result, window, cx| {
                this.finish_restore_revert(finish_key, result, window, cx)
            },
            window,
            cx,
        );
        if started {
            self.branching.restoring.insert(key);
            cx.notify();
        }
    }

    pub(crate) fn finish_restore_revert(
        &mut self,
        key: SessionKey,
        result: Option<Result<(), TransportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.branching.restoring.remove(&key);
        if key.runtime != self.runtime {
            return;
        }
        match result {
            Some(Ok(())) => {
                let event = ServerEvent::SessionPatched {
                    session_id: key.session.clone(),
                    patch: SessionPatch {
                        revert: Some(None),
                        ..SessionPatch::default()
                    },
                };
                self.apply_server_events(vec![event], window, cx);
                if let Some(directory) = self
                    .sessions
                    .get(&key.session)
                    .and_then(SessionRecord::resolved_directory)
                {
                    self.refresh_git_status_soon(&directory, cx);
                }
            }
            Some(Err(error)) => self.branching_error(
                "chat.revertPopover.toast.restoreFailed",
                Some(&error),
                window,
                cx,
            ),
            None => {}
        }
        self.remeasure_transcript(&key.session);
        cx.notify();
    }

    pub(crate) fn toggle_reverted_list(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&session);
        let Some(marker) = self.revert_marker(&session) else {
            return;
        };
        let entry = (key, marker);
        self.branching.strip_expanded = match self.branching.strip_expanded.take() {
            Some(open) if open == entry => None,
            _ => Some(entry),
        };
        cx.notify();
    }

    /// The next prompt committed the revert: the server deleted the cut and
    /// everything after it without per-message removals.
    pub(crate) fn on_revert_committed(
        &mut self,
        session: &SessionId,
        to: &MessageId,
        cx: &mut Context<Self>,
    ) {
        let marker = self
            .sessions
            .get(session)
            .and_then(|record| record.revert.clone());
        if let Some(open) = self.open_sessions.get_mut(session) {
            let change = open.transcript.truncate_from(to, marker.as_ref());
            crate::server_sync::apply_list_change(open, &change, cx);
            // No pre-commit page may settle send attempts or history state.
            // Its boundary can be outside the entire fetched page.
            open.history_generation = open.history_generation.wrapping_add(1);
            open.earlier = crate::workspace::EarlierHistory::Idle;
        }
        self.load_history(session, cx);
    }

    // --- /btw ---------------------------------------------------------------

    /// The selected session's `/btw` panel, if it has one.
    pub(crate) fn btw_panel(&self) -> Option<BtwPanel> {
        let parent = self.selected.as_ref()?;
        let key = self.mutation_key(parent);
        let local = self.branching.btw.get(&key).copied().unwrap_or_default();
        let fork = self
            .sessions
            .get(parent)?
            .btw
            .link
            .clone()
            .filter(|fork| self.session(fork).is_some());
        match fork {
            Some(fork) if !local.destroying => Some(BtwPanel::Open {
                boundary: self.sessions.get(&fork)?.btw.boundary.clone(),
                fork,
                local,
            }),
            _ if local.creating => Some(BtwPanel::Creating),
            _ => None,
        }
    }

    /// Loads the open panel's side conversation, which is never selected.
    pub(crate) fn open_btw_fork(&mut self, cx: &mut Context<Self>) {
        if let Some(BtwPanel::Open { fork, .. }) = self.btw_panel()
            && !self.open_sessions.contains_key(&fork)
        {
            self.ensure_open(&fork, cx);
        }
    }

    /// Whether the composer sends into the open `/btw` panel.
    pub(crate) fn btw_takes_composer(&self) -> bool {
        matches!(
            self.btw_panel(),
            Some(BtwPanel::Open { local, .. }) if !local.collapsed
        )
    }

    /// `/btw <question>`: forks the selected session at its last finished
    /// turn into a hidden side conversation, links the parent and sends the
    /// question there with the boundary instruction (`startBtwSession`). A
    /// failure after the fork unlinks the parent and deletes the fork.
    pub(crate) fn start_btw(
        &mut self,
        question: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(parent) = self.selected.clone() else {
            return false;
        };
        if question.trim().is_empty() {
            self.branching_error("chat.btw.toast.emptyArgument", None, window, cx);
            return false;
        }
        let key = self.mutation_key(&parent);
        if self
            .branching
            .btw
            .get(&key)
            .is_some_and(|local| local.creating)
        {
            return false;
        }
        let Some((_, selection)) = self.send_selection() else {
            return false;
        };
        let fork_point = self
            .open_sessions
            .get(&parent)
            .and_then(|open| branching::btw_fork_point(open.transcript.messages()));
        let (context_id, _) = self.message_ids.next();
        let (prompt_id, _) = self.message_ids.next();
        let prompt = Prompt {
            message_id: prompt_id,
            text: question.clone(),
            parts: PromptParts {
                context: vec![SyntheticContext {
                    id: context_id,
                    text: BTW_BOUNDARY_INSTRUCTION.to_owned(),
                    description: None,
                    metadata: None,
                }],
                ..PromptParts::default()
            },
            model: selection.model,
            agent: selection.agent_name,
            delivery: None,
        };
        let title = format!("btw: {question}");
        let draft = self.capture_btw_draft(cx);
        let id = parent.clone();
        let finish_key = key.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    let mut fork = client.fork_session(&id, fork_point.as_ref()).await?;
                    let result: Result<MetadataFacts, TransportError> = async {
                        // The boundary is the newest message the fork holds;
                        // the fork point stands in when that read fails.
                        let boundary = client
                            .latest_messages(&fork.id, 1)
                            .await
                            .ok()
                            .and_then(|page| page.messages.last().map(|m| m.info.id.clone()))
                            .or(fork_point);
                        let facts = client
                            .mark_btw_fork(&fork.id, &id, boundary.as_ref())
                            .await?;
                        fork.overlay = facts.overlay;
                        fork.kind = facts.kind;
                        fork.btw = facts.btw;
                        if let Ok(renamed) = client.rename_session(&fork.id, &title).await {
                            fork.title = renamed.title;
                        }
                        let parent_facts = client.link_btw(&id, &fork.id).await?;
                        if let Err(error) = client.send_prompt(&fork.id, &prompt).await {
                            let _ = client.unlink_btw(&id, &fork.id).await;
                            return Err(error.source);
                        }
                        Ok(parent_facts)
                    }
                    .await;
                    match result {
                        Ok(parent_facts) => Ok((fork, parent_facts)),
                        Err(error) => {
                            let _ = client.delete_session(&fork.id).await;
                            Err(error)
                        }
                    }
                })
            },
            move |this, result, window, cx| {
                this.finish_start_btw(finish_key, draft, result, window, cx)
            },
            window,
            cx,
        );
        if started {
            self.branching.btw.insert(
                key,
                BtwLocal {
                    creating: true,
                    ..BtwLocal::default()
                },
            );
            cx.notify();
        }
        started
    }

    pub(crate) fn finish_start_btw(
        &mut self,
        key: SessionKey,
        draft: Option<BtwDraft>,
        result: Option<Result<(SessionRecord, MetadataFacts), TransportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.branching.btw.remove(&key);
        if matches!(result, Some(Ok(_)))
            && let Some(draft) = draft
        {
            self.clear_sent_btw_draft(draft, window, cx);
        }
        if key.runtime != self.runtime {
            return;
        }
        match result {
            Some(Ok((fork, parent))) => {
                let fork_id = fork.id.clone();
                // The marker applies even over a copy an event already listed.
                let marker = ServerEvent::SessionPatched {
                    session_id: fork_id.clone(),
                    patch: SessionPatch {
                        title: Some(fork.title.clone()),
                        metadata: Some(MetadataFacts {
                            overlay: fork.overlay,
                            kind: fork.kind,
                            btw: fork.btw.clone(),
                            goal: fork.goal.clone(),
                        }),
                        ..SessionPatch::default()
                    },
                };
                self.apply_server_events(
                    vec![
                        ServerEvent::SessionCreated(fork),
                        marker,
                        ServerEvent::SessionPatched {
                            session_id: key.session.clone(),
                            patch: SessionPatch {
                                metadata: Some(parent),
                                ..SessionPatch::default()
                            },
                        },
                    ],
                    window,
                    cx,
                );
                self.ensure_open(&fork_id, cx);
            }
            Some(Err(error)) => {
                self.branching_error("chat.btw.toast.createFailed", Some(&error), window, cx);
            }
            None => {}
        }
        cx.notify();
    }

    /// Sends the composer's text into the open side conversation, with the
    /// boundary instruction, as the web composer does while the panel owns it.
    /// Text stays in its persisted draft until the server accepts it.
    pub(crate) fn send_btw_follow_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(BtwPanel::Open { fork, local, .. }) = self.btw_panel() else {
            return;
        };
        let Some(parent) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&parent);
        if local.sending || local.collapsed {
            return;
        }
        let text = self.composer.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let Some((_, selection)) = self.send_selection() else {
            return;
        };
        let (context_id, _) = self.message_ids.next();
        let (prompt_id, _) = self.message_ids.next();
        let prompt = Prompt {
            message_id: prompt_id,
            text: text.clone(),
            parts: PromptParts {
                context: vec![SyntheticContext {
                    id: context_id,
                    text: BTW_BOUNDARY_INSTRUCTION.to_owned(),
                    description: None,
                    metadata: None,
                }],
                ..PromptParts::default()
            },
            model: selection.model,
            agent: selection.agent_name,
            delivery: None,
        };
        let finish_key = key.clone();
        let draft = self.capture_btw_draft(cx);
        let started = self.spawn_session_task(
            move |client| Box::pin(async move { client.send_prompt(&fork, &prompt).await }),
            move |this, result, window, cx| {
                if let Some(local) = this.branching.btw.get_mut(&finish_key) {
                    local.sending = false;
                }
                if matches!(result, Some(Ok(_)))
                    && let Some(draft) = draft
                {
                    this.clear_sent_btw_draft(draft, window, cx);
                }
                if !matches!(result, Some(Ok(_))) && finish_key.runtime == this.runtime {
                    this.notify_mutation_failure("chat.nativeRecovery.sendFailed", window, cx);
                }
            },
            window,
            cx,
        );
        if started {
            self.branching.btw.entry(key).or_default().sending = true;
            cx.notify();
        }
    }

    pub(crate) fn set_btw_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        let Some(parent) = self.selected.clone() else {
            return;
        };
        let key = self.mutation_key(&parent);
        self.branching.btw.entry(key).or_default().collapsed = collapsed;
        cx.notify();
    }

    /// Escape collapses an open panel and hands the composer back to the
    /// parent. Returns whether it acted.
    pub(crate) fn escape_btw(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.btw_takes_composer() {
            return false;
        }
        self.set_btw_collapsed(true, cx);
        true
    }

    /// Deletes the side conversation. The panel closes at once; the parent
    /// is unlinked first, so the close holds even when the delete fails
    /// (`destroyBtwSession`).
    pub(crate) fn destroy_btw(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(parent), Some(BtwPanel::Open { fork, .. })) =
            (self.selected.clone(), self.btw_panel())
        else {
            return;
        };
        let key = self.mutation_key(&parent);
        let finish_key = key.clone();
        let fork_id = fork.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    let unlinked = client.unlink_btw(&parent, &fork).await.ok();
                    (unlinked, client.delete_session(&fork).await)
                })
            },
            move |this, result, window, cx| {
                this.branching.btw.remove(&finish_key);
                if finish_key.runtime != this.runtime {
                    return;
                }
                let Some((unlinked, deleted)) = result else {
                    return;
                };
                let mut events = Vec::new();
                if let Some(facts) = unlinked {
                    events.push(ServerEvent::SessionPatched {
                        session_id: finish_key.session.clone(),
                        patch: SessionPatch {
                            metadata: Some(facts),
                            ..SessionPatch::default()
                        },
                    });
                }
                if deleted.is_ok() {
                    events.push(ServerEvent::SessionDeleted {
                        session_id: fork_id.clone(),
                    });
                }
                this.apply_server_events(events, window, cx);
                if let Err(error) = deleted {
                    this.branching_error("chat.btw.toast.destroyFailed", Some(&error), window, cx);
                }
            },
            window,
            cx,
        );
        if started {
            self.branching.btw.entry(key).or_default().destroying = true;
            cx.notify();
        }
    }

    /// Keeps the side conversation as an ordinary session and opens it
    /// (`promoteBtwSession`).
    pub(crate) fn promote_btw(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(parent), Some(BtwPanel::Open { fork, local, .. })) =
            (self.selected.clone(), self.btw_panel())
        else {
            return;
        };
        if local.promoting {
            return;
        }
        let key = self.mutation_key(&parent);
        let finish_key = key.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move {
                    client.promote_btw(&parent, &fork).await?;
                    Ok::<_, TransportError>((
                        client.session(&parent).await?,
                        client.session(&fork).await?,
                    ))
                })
            },
            move |this, result, window, cx| {
                if let Some(local) = this.branching.btw.get_mut(&finish_key) {
                    local.promoting = false;
                }
                if finish_key.runtime != this.runtime {
                    return;
                }
                match result {
                    Some(Ok((parent, fork))) => {
                        this.branching.btw.remove(&finish_key);
                        let fork_id = fork.id.clone();
                        this.apply_server_events(
                            vec![
                                ServerEvent::SessionCreated(parent),
                                ServerEvent::SessionCreated(fork),
                            ],
                            window,
                            cx,
                        );
                        this.select_session(fork_id, window, cx);
                    }
                    Some(Err(error)) => this.branching_error(
                        "chat.btw.toast.promoteFailed",
                        Some(&error),
                        window,
                        cx,
                    ),
                    None => {}
                }
            },
            window,
            cx,
        );
        if started {
            self.branching.btw.entry(key).or_default().promoting = true;
            cx.notify();
        }
    }

    // --- Subagents ----------------------------------------------------------

    /// Opens a subagent's child session. A child this client has not listed
    /// is read first.
    pub(crate) fn open_subtask(
        &mut self,
        child: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session(&child).is_some() {
            self.select_session(child, window, cx);
            return;
        }
        let runtime = self.runtime.clone();
        let navigation = self.navigation_revision;
        self.spawn_session_task(
            move |client| Box::pin(async move { client.session(&child).await }),
            move |this, result, window, cx| {
                this.finish_open_subtask(runtime, navigation, result, window, cx)
            },
            window,
            cx,
        );
    }

    pub(crate) fn finish_open_subtask(
        &mut self,
        runtime: RuntimeId,
        navigation: u64,
        result: Option<Result<SessionRecord, TransportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if runtime != self.runtime {
            return;
        }
        match result {
            Some(Ok(record)) => {
                let id = record.id.clone();
                self.apply_server_events(vec![ServerEvent::SessionCreated(record)], window, cx);
                if self.navigation_revision == navigation {
                    self.select_session(id, window, cx);
                }
            }
            Some(Err(error)) => self.branching_error(
                "chat.container.sessionLoadError.title",
                Some(&error),
                window,
                cx,
            ),
            _ => {}
        }
    }

    /// The selected session's parent, when it has one this client lists.
    pub(crate) fn parent_session(&self) -> Option<&SessionRecord> {
        let parent = self.selected_session()?.parent_id.as_ref()?;
        self.session(parent)
    }

    pub(crate) fn return_to_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.parent_session().map(|parent| parent.id.clone()) {
            self.select_session(parent, window, cx);
        }
    }

    /// Stops a running subagent: a note in the parent first, then the child's
    /// interrupt (`stopSubagent`).
    pub(crate) fn stop_subagent(
        &mut self,
        parent: SessionId,
        child: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.mutation_key(&child);
        if self.branching.stopping.contains(&key) {
            return;
        }
        let (note, _) = self.message_ids.next();
        let finish_key = key.clone();
        let started = self.spawn_session_task(
            move |client| {
                Box::pin(async move { client.stop_subagent(&parent, &child, &note, None).await })
            },
            move |this, result, window, cx| {
                this.finish_stop_subagent(finish_key, result, window, cx);
            },
            window,
            cx,
        );
        if started {
            self.branching.stopping.insert(key);
            cx.notify();
        }
    }

    pub(crate) fn finish_stop_subagent(
        &mut self,
        key: SessionKey,
        result: Option<Result<(), TransportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.branching.stopping.remove(&key);
        if key.runtime == self.runtime && matches!(result, Some(Err(_))) {
            self.branching_error("chat.toolPart.subagent.stopFailed", None, window, cx);
        }
    }

    /// Runs `/fork` or `/btw` from the composer. Returns whether the text was
    /// a branching command. BTW text stays until the server accepts it.
    pub(crate) fn run_branch_command(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.is_synthetic() || self.selected.is_none() {
            return false;
        }
        let Some(command) = parse_command(&self.composer.read(cx).value()) else {
            return false;
        };
        self.save_composer_draft(cx);
        self.queue_persistence(cx);
        match command {
            BranchCommand::Fork => {
                self.composer
                    .update(cx, |composer, cx| composer.set_value("", window, cx));
                self.save_composer_draft(cx);
                self.queue_persistence(cx);
                self.fork_after_last_turn(window, cx);
            }
            BranchCommand::Btw(question) => {
                self.start_btw(question, window, cx);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_the_first_word() {
        assert_eq!(parse_command("/fork"), Some(BranchCommand::Fork));
        assert_eq!(
            parse_command("  /btw what is  this?\n"),
            Some(BranchCommand::Btw("what is  this?".to_owned()))
        );
        assert_eq!(
            parse_command("/btw"),
            Some(BranchCommand::Btw(String::new()))
        );
        assert_eq!(parse_command("/forked"), None);
        assert_eq!(parse_command("please /fork"), None);
    }
}
