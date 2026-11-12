//! Hydration, save barriers and normal-close draining for private native state.

use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::{AnyWindowHandle, Context, Task, Window};
use openchamber_native_core::draft::{Draft, DraftKey, DraftStore};
use openchamber_native_core::draft_content::{DraftFile, DraftFileContent};

use crate::attachment_catalog::{AttachmentCatalog, PreparationHold, PreparedAttachment};
use crate::attachment_spool::BlobDescriptor;
use crate::journal_worker::{
    CommitOutcome, JournalFailure, JournalReceipt, JournalResult, JournalWorker,
};
use crate::mutation_journal::{JournalError, JournalSnapshot, JournalView, LoadedJournal};
use crate::recovery_view::StorageNotice;
use crate::send_attempt::SendAttempts;
use crate::workspace::Workspace;

const SAVE_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub(crate) enum StorageIssue {
    Failure(JournalFailure),
    WriteRejected,
    Uncertain,
}

#[derive(Default)]
enum Status {
    #[default]
    Disabled,
    Loading,
    Ready,
    Fault {
        loaded: bool,
        issue: StorageIssue,
    },
    Closing,
}

#[derive(Default)]
pub(crate) struct StatePersistence {
    status: Status,
    worker: Option<JournalWorker>,
    path: Option<PathBuf>,
    revision: u64,
    epoch: u64,
    last_result: u64,
    opening: Option<Task<()>>,
    debounce: Option<Task<()>>,
    saving: Option<Task<()>>,
    closing: Option<Task<()>>,
    accepted_close: Option<Task<()>>,
    composer_close: Option<Task<()>>,
    settings_close: Option<Task<()>>,
    knowledge_close: Option<Task<()>>,
    composer_hydration_close: bool,
    native_root_close: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct WriteStamp {
    epoch: u64,
    revision: u64,
}

impl StatePersistence {
    fn loaded(&self) -> bool {
        matches!(
            self.status,
            Status::Ready | Status::Closing | Status::Fault { loaded: true, .. }
        )
    }
}

impl Workspace {
    pub(crate) fn attachment_worker(&self) -> Option<&JournalWorker> {
        self.persistence.worker.as_ref()
    }

    pub(crate) fn persistence_loaded(&self) -> bool {
        self.persistence.loaded()
    }

    pub(crate) fn persistence_ready(&self) -> bool {
        matches!(self.persistence.status, Status::Ready)
    }

    pub(crate) fn persistence_closing(&self) -> bool {
        matches!(self.persistence.status, Status::Closing)
    }

    pub(crate) fn close_in_progress(&self) -> bool {
        self.persistence_closing()
            || self.persistence.accepted_close.is_some()
            || self.persistence.native_root_close
            || self.composer_close_in_progress()
            || self.persistence.settings_close.is_some()
            || self.persistence.knowledge_close.is_some()
            || self.general_close.is_some()
            || self.appearance_close.is_some()
            || self.host_close.is_some()
            || self.attachment_storage_close.is_some()
            || self.effort_close.is_some()
            || self.worktree_close.is_some()
            || self.effort_preferences.is_closing()
    }

    pub(crate) fn composer_close_in_progress(&self) -> bool {
        self.persistence.composer_close.is_some() || self.persistence.composer_hydration_close
    }

    pub(crate) fn composer_hydration_close_in_progress(&self) -> bool {
        self.persistence.composer_hydration_close
    }

    /// Local document completion resumes a waiting close without starting another
    /// read or polling. A failed read reaches freeze's ordinary refusal path.
    pub(crate) fn resume_composer_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.persistence.composer_hydration_close || self.composer_documents_pending() {
            return;
        }
        self.persistence.composer_hydration_close = false;
        self.request_close(window, cx);
    }

    pub(crate) fn native_root_actors_pending(&self) -> bool {
        self.queue.enqueues.has_native_root_holds()
            || self.queue.prefixes.has_native_root_holds()
            || self.queue.local_sends.has_native_root_holds()
    }

    /// Actor callbacks run on the foreground before the journal owner can seal.
    /// A new unwritten cleanup request keeps the window and writer for retry.
    pub(crate) fn resume_native_root_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.persistence.native_root_close || self.native_root_actors_pending() {
            return;
        }
        self.persistence.native_root_close = false;
        if self.has_pending_attachment_retirements() {
            self.unfreeze_host_manager(cx);
            self.unfreeze_appearance(cx);
            self.unfreeze_general(cx);
            self.notify_mutation_failure("chat.nativeRecovery.storageFailed", window, cx);
            cx.notify();
            return;
        }
        self.request_close(window, cx);
    }

    pub(crate) fn composer_editable(&self) -> bool {
        (self.is_synthetic() || self.persistence.loaded())
            && !self.close_in_progress()
            && self
                .selected
                .as_ref()
                .is_none_or(|session| self.prompt_allowed(&self.mutation_key(session)))
            && !matches!(self.persistence.status, Status::Closing | Status::Loading)
            && !self.goal_arming_selected()
            && !self.selected.as_ref().is_some_and(|session| {
                self.send_attempts
                    .session(&self.mutation_key(session))
                    .any(|attempt| attempt.admission == crate::send_attempt::Admission::Preparing)
            })
    }

    pub(crate) fn persistence_notice(&self) -> Option<StorageNotice> {
        match &self.persistence.status {
            Status::Disabled | Status::Ready => None,
            Status::Closing => Some(StorageNotice::Closing),
            Status::Loading => Some(StorageNotice::Loading),
            Status::Fault {
                issue: StorageIssue::WriteRejected,
                ..
            } => Some(StorageNotice::WriteRejected),
            Status::Fault {
                issue: StorageIssue::Uncertain,
                ..
            } => Some(StorageNotice::SyncUncertain),
            Status::Fault {
                issue: StorageIssue::Failure(error),
                ..
            } => Some(match error {
                JournalFailure::Locked => StorageNotice::Locked,
                JournalFailure::Invalid(_) | JournalFailure::UnsupportedVersion(_) => {
                    StorageNotice::Invalid
                }
                _ => StorageNotice::Io,
            }),
        }
    }

    pub(crate) fn start_persistence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_synthetic()
            || self.host_recovery.is_some()
            || !matches!(self.persistence.status, Status::Disabled)
        {
            return;
        }
        let Some(path) =
            crate::install_id::config_directory().map(|path| path.join("drafts-and-sends.json"))
        else {
            self.persistence.status = Status::Fault {
                loaded: false,
                issue: StorageIssue::Failure(JournalFailure::Invalid(
                    "native profile is unavailable",
                )),
            };
            return;
        };
        self.open_persistence(path, window, cx);
    }

    pub(crate) fn open_persistence(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Once hydrated, memory may be newer than disk. Reopening cannot prove
        // that another process did not write while our lock was released.
        if self.persistence.loaded() {
            self.persistence.status = Status::Fault {
                loaded: true,
                issue: StorageIssue::Failure(JournalFailure::WorkerStopped),
            };
            cx.notify();
            return;
        }
        self.persistence.worker = None;
        self.persistence.path = Some(path.clone());
        self.persistence.status = Status::Loading;
        self.persistence.epoch = self.persistence.epoch.wrapping_add(1);
        let epoch = self.persistence.epoch;
        let (worker, ready) = JournalWorker::start(path);
        self.persistence.worker = Some(worker);
        self.persistence.opening = Some(cx.spawn_in(window, async move |this, cx| {
            let loaded = ready.await.unwrap_or(Err(JournalFailure::WorkerStopped));
            let _ = this.update_in(cx, |this, window, cx| {
                if this.persistence.epoch != epoch {
                    return;
                }
                match loaded {
                    Ok(loaded) => {
                        let snapshot = match loaded {
                            LoadedJournal::Missing => JournalSnapshot::default(),
                            LoadedJournal::Present(snapshot) => *snapshot,
                        };
                        let attempts = match SendAttempts::from_snapshot(snapshot.attempts) {
                            Ok(attempts) => attempts,
                            Err(reason) => {
                                this.persistence.worker = None;
                                this.persistence.status = Status::Fault {
                                    loaded: false,
                                    issue: StorageIssue::Failure(JournalFailure::Invalid(reason)),
                                };
                                cx.notify();
                                return;
                            }
                        };
                        let mut drafts = DraftStore::new();
                        for (key, draft) in snapshot.drafts {
                            drafts.save(key, draft);
                        }
                        this.drafts = drafts;
                        this.composer_selections = openchamber_native_core::composer_selection::ComposerSelections::restore(snapshot.selections);
                        this.send_attempts = attempts;
                        this.creations =
                            crate::create_attempt::CreateAttempts::restore(snapshot.creations);
                        this.attachments = snapshot.attachments;
                        this.attachment_preparations = snapshot.preparations;
                        this.retired_attachment_blobs = snapshot.retired_blobs;
                        this.queue_storage = snapshot.queue;
                        this.queue.enqueues.restore(&this.queue_storage);
                        this.queue.local_sends.restore(&this.queue_storage);
                        this.starters.captures = crate::starter_capture::StarterCaptures::restore(snapshot.starter_captures);
                        let drained = this.starters.captures.drain(&mut this.drafts);
                        for capture in drained.restored {
                            capture.restore_selection(&mut this.composer_selections);
                            if let Some(mode) = capture.permission_mode {
                                this.permission_modes.drafts.entry(capture.owner.runtime().clone()).or_insert(mode);
                            }
                        }
                        this.restore_draft_previews(cx);
                        this.restore_composer_draft(window, cx);
                        // A Chat draft stored before the restart names its folder.
                        this.settle_chat_folder(window, cx);
                        this.persistence.status = Status::Ready;
                    }
                    Err(error) => {
                        this.persistence.worker = None;
                        this.persistence.status = Status::Fault {
                            loaded: false,
                            issue: StorageIssue::Failure(error),
                        }
                    }
                }
                cx.notify();
            });
        }));
    }

    fn journal_drafts(&self) -> Vec<(&DraftKey, &Draft)> {
        // During the durable barrier the field still displays the submitted
        // text. The attempt owns that copy in storage; a later identical draft
        // typed after dispatch is a separate value and remains in the journal.
        let preparing: HashMap<_, _> = self
            .send_attempts
            .iter()
            .filter(|(_, attempt)| {
                attempt.admission == crate::send_attempt::Admission::Preparing
                    && attempt.origin == crate::send_attempt::SendOrigin::Existing
            })
            .map(|(_, attempt)| (&attempt.owner, &attempt.draft))
            .collect();
        self.drafts
            .iter()
            .filter(|(key, draft)| {
                preparing
                    .get(key)
                    .is_none_or(|captured| !captured.same_content(draft))
            })
            .collect()
    }

    fn validate_attachment_snapshot(
        &self,
        attachments: &AttachmentCatalog,
        preparations: &[PreparationHold],
        added_files: Option<(&DraftKey, &Draft)>,
    ) -> Result<(), JournalError> {
        let selections = self.composer_selections.snapshot();
        let empty = Draft::default();
        let mut drafts = self.journal_drafts();
        if let Some((owner, _)) = added_files {
            // Appended files make a preparing send's former duplicate draft an
            // independent owner again, exactly as the next save will see it.
            if !drafts.iter().any(|(key, _)| *key == owner) {
                drafts.push((owner, self.drafts.get(owner).unwrap_or(&empty)));
            }
        }
        JournalView {
            drafts,
            attempts: self.send_attempts.iter().collect(),
            creations: self.creations.iter().collect(),
            selections: &selections,
            attachments,
            preparations,
            retired_blobs: &self.retired_attachment_blobs,
            starter_captures: self.starters.captures.iter().collect(),
            added_files,
            queue: &self.queue_storage,
        }
        .validate()
    }

    /// Checks a newly captured queue send against all other retained owners
    /// before its first save or server reservation can acquire ownership.
    pub(crate) fn validate_queue_intent_admission(&self) -> Result<(), JournalError> {
        self.validate_attachment_snapshot(&self.attachments, &self.attachment_preparations, None)
    }

    /// Runs the canonical full-snapshot preflight against a proposed queue ledger.
    pub(crate) fn validate_queue_candidate(
        &self,
        queue: &crate::queue_storage::QueueStorage,
    ) -> Result<(), JournalError> {
        let selections = self.composer_selections.snapshot();
        JournalView {
            drafts: self.journal_drafts(),
            attempts: self.send_attempts.iter().collect(),
            creations: self.creations.iter().collect(),
            selections: &selections,
            attachments: &self.attachments,
            preparations: &self.attachment_preparations,
            retired_blobs: &self.retired_attachment_blobs,
            starter_captures: self.starters.captures.iter().collect(),
            added_files: None,
            queue,
        }
        .validate()
    }

    /// Admits a complete source's initial holds without acquiring any ownership.
    pub(crate) fn validate_attachment_preparation_admission(
        &self,
        holds: &[PreparationHold],
    ) -> Result<(), JournalError> {
        if holds.is_empty() {
            return Err(JournalError::Invalid("empty attachment preparation"));
        }
        let mut preparations = self.attachment_preparations.clone();
        preparations.extend_from_slice(holds);
        self.validate_attachment_snapshot(&self.attachments, &preparations, None)
    }

    /// Checks the complete canonical v7 snapshot before atomic source publication.
    pub(crate) fn validate_attachment_bundle_publication(
        &self,
        owner: &DraftKey,
        members: &[(&PreparationHold, BlobDescriptor, DraftFile)],
    ) -> Result<(), JournalError> {
        if members.is_empty() {
            return Err(JournalError::Invalid("empty attachment publication"));
        }
        let mut ids = HashSet::new();
        let mut attachments = self.attachments.clone();
        let mut additions = Draft::default();
        for (hold, descriptor, file) in members {
            if &hold.owner != owner
                || file.id != hold.id
                || descriptor.id() != &hold.blob
                || !matches!(&file.content, DraftFileContent::Prepared)
                || !ids.insert(&hold.id)
                || self.attachments.contains_key(&hold.id)
                || self
                    .attachment_preparations
                    .iter()
                    .filter(|existing| existing.id == hold.id)
                    .count()
                    != 1
                || !self.attachment_preparations.contains(hold)
            {
                return Err(JournalError::Invalid(
                    "attachment publication differs from exact preparation",
                ));
            }
            attachments.insert(
                hold.id.clone(),
                PreparedAttachment {
                    blob: *descriptor,
                    staging: None,
                },
            );
            additions.content.files.push(file.clone());
        }
        let preparations: Vec<_> = self
            .attachment_preparations
            .iter()
            .filter(|hold| !members.iter().any(|(expected, _, _)| *expected == *hold))
            .cloned()
            .collect();
        self.validate_attachment_snapshot(&attachments, &preparations, Some((owner, &additions)))
    }

    /// Full snapshots are collected only at a save boundary, not on each keystroke.
    pub(crate) fn persistence_barrier(
        &mut self,
        cx: &gpui_kit::App,
    ) -> Option<(WriteStamp, JournalReceipt)> {
        if !self.persistence.loaded()
            || matches!(self.persistence.status, Status::Closing | Status::Loading)
        {
            return None;
        }
        self.save_composer_draft(cx);
        let snapshot = JournalSnapshot {
            drafts: self
                .journal_drafts()
                .into_iter()
                .map(|(key, draft)| (key.clone(), draft.clone()))
                .collect(),
            attempts: self.send_attempts.snapshot(),
            creations: self.creations.snapshot(),
            selections: self.composer_selections.snapshot(),
            attachments: self.attachments.clone(),
            preparations: self.attachment_preparations.clone(),
            retired_blobs: self.retired_attachment_blobs.clone(),
            starter_captures: self.starters.captures.snapshot(),
            queue: self.queue_storage.clone(),
        };
        let worker = self.persistence.worker.as_ref()?;
        self.persistence.revision += 1;
        Some((
            WriteStamp {
                epoch: self.persistence.epoch,
                revision: self.persistence.revision,
            },
            worker.submit(snapshot),
        ))
    }

    pub(crate) fn record_persistence(
        &mut self,
        stamp: WriteStamp,
        result: &JournalResult,
        cx: &mut Context<Self>,
    ) -> bool {
        let durable = matches!(result, Ok(CommitOutcome::Durable));
        if stamp.epoch != self.persistence.epoch {
            return false;
        }
        if stamp.revision >= self.persistence.last_result
            && !matches!(self.persistence.status, Status::Closing)
        {
            self.persistence.last_result = stamp.revision;
            self.persistence.status = match result {
                Ok(CommitOutcome::Durable) => Status::Ready,
                Ok(CommitOutcome::DirectorySyncUncertain(_)) => Status::Fault {
                    loaded: true,
                    issue: StorageIssue::Uncertain,
                },
                Err(JournalFailure::Invalid(_)) => Status::Fault {
                    loaded: true,
                    issue: StorageIssue::WriteRejected,
                },
                Err(error) => Status::Fault {
                    loaded: true,
                    issue: StorageIssue::Failure(error.clone()),
                },
            };
            cx.notify();
        }
        durable && !matches!(self.persistence.status, Status::Closing)
    }

    pub(crate) fn queue_persistence(&mut self, cx: &mut Context<Self>) {
        if !self.persistence.loaded()
            || matches!(self.persistence.status, Status::Closing | Status::Loading)
        {
            return;
        }
        self.persistence.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let _ = this.update(cx, |this, cx| this.start_background_save(cx));
        }));
    }

    pub(crate) fn start_background_save(&mut self, cx: &mut Context<Self>) {
        let Some((revision, saved)) = self.persistence_barrier(cx) else {
            return;
        };
        self.persistence.saving = Some(cx.spawn(async move |this, cx| {
            let result = saved.await.unwrap_or(Err(JournalFailure::WorkerStopped));
            let _ = this.update(cx, |this, cx| {
                this.record_persistence(revision, &result, cx);
            });
        }));
    }

    pub(crate) fn retry_persistence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.retry_composer_submissions(None, window, cx);
        if self.persistence.loaded() && self.persistence.worker.is_some() {
            self.start_background_save(cx);
        } else if let Some(path) = self.persistence.path.clone() {
            self.open_persistence(path, window, cx);
        } else {
            self.start_persistence(window, cx);
        }
    }

    pub(crate) fn flush_before_quit(&mut self, cx: &mut Context<Self>) -> Option<JournalReceipt> {
        self.cancel_attachment_staging(cx);
        self.attachment_retirement.cancel();
        self.persistence.debounce = None;
        let (_, saved) = self.persistence_barrier(cx)?;
        // The flush receipt covers this save and every earlier accepted write.
        drop(saved);
        self.persistence.worker.as_ref().map(JournalWorker::flush)
    }

    pub(crate) fn take_appearance_flush(
        &mut self,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Option<Task<()>> {
        // A normal-close waiter uses the foreground session that quit now blocks.
        // The appearance owner retains its independent disk operation for this drain.
        self.appearance_close = None;
        if let Some(settings) = &self.settings {
            // Quit observers run before GPUI removes windows. Without the
            // input's window, only already accepted preferences may be flushed.
            let _ = window.update(cx, |_, window, cx| {
                settings.update(cx, |settings, cx| settings.prepare_close(window, cx));
            });
        }
        let appearance = cx
            .try_global::<crate::appearance_view::AppearanceOwner>()?
            .0
            .clone();
        appearance.update(cx, |appearance, cx| appearance.take_quit_write(cx))
    }

    pub(crate) fn take_general_flush(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.general_close = None;
        let general = self.settings.as_ref()?.read(cx).general.clone();
        general.update(cx, |general, cx| general.take_quit_write(cx))
    }

    pub(crate) fn take_panel_quit_flush(
        &mut self,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Vec<Task<()>> {
        self.persistence.settings_close = None;
        self.persistence.knowledge_close = None;
        let panels = self.panels.all_knowledge_views();
        let client = self
            .connection
            .as_ref()
            .and_then(|connection| connection.client.clone());
        let _ = window.update(cx, |_, window, cx| {
            if let Some(settings) = &self.settings {
                settings.update(cx, |settings, cx| settings.prepare_close(window, cx));
            }
            for panel in &panels {
                panel.update(cx, |panel, cx| {
                    if let Some(client) = &client {
                        panel.refresh_write_client(&self.runtime, client);
                    }
                    panel.prepare_close(window, cx);
                });
            }
        });
        let mut drains = Vec::new();
        if let Some(settings) = &self.settings
            && let Some(drain) = settings.update(cx, |settings, cx| settings.take_quit_write(cx))
        {
            drains.push(drain);
        }
        for panel in panels {
            if let Some(drain) = panel.update(cx, |panel, cx| panel.take_quit_write(cx)) {
                drains.push(drain);
            }
        }
        drains
    }

    pub(crate) fn take_profile_quit_flush(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.host_close = None;
        self.host_manager
            .as_ref()?
            .update(cx, |manager, cx| manager.take_quit_write(cx))
    }

    fn unfreeze_general(&mut self, cx: &mut Context<Self>) {
        self.persistence.native_root_close = false;
        self.persistence.composer_hydration_close = false;
        self.unfreeze_composer_documents();
        self.cancel_effort_close();
        self.attachment_storage.resume_accepting();
        if let Some(settings) = &self.settings {
            settings.update(cx, |settings, cx| settings.cancel_close(cx));
            settings.read(cx).general.clone().update(cx, |general, cx| {
                general.cancel_close(cx);
            });
        }
        for panel in self.panels.all_knowledge_views() {
            panel.update(cx, |panel, cx| panel.cancel_close(cx));
        }
    }

    pub(crate) fn unfreeze_appearance(&mut self, cx: &mut Context<Self>) {
        if let Some(appearance) = cx.try_global::<crate::appearance_view::AppearanceOwner>() {
            appearance.0.clone().update(cx, |appearance, cx| {
                appearance.cancel_close();
                cx.notify();
            });
        }
    }

    /// Retain admission keys while joining accepted callbacks and their follow-ups.
    fn drain_accepted_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut pending = Vec::new();
        for tasks in [
            &mut self.creation_tasks,
            &mut self.mutation_tasks,
            &mut self.abort_tasks,
        ] {
            for task in tasks.values_mut() {
                if !task.is_ready() {
                    pending.push(mem::replace(task, Task::ready(())));
                }
            }
        }
        pending.extend(self.permission_modes.take_close_tasks());
        pending.extend(self.recovery.take_close_tasks());
        if pending.is_empty() {
            return false;
        }
        self.persistence.accepted_close = Some(cx.spawn_in(window, async move |this, cx| {
            for task in pending {
                task.await;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.persistence.accepted_close = None;
                // Callbacks may have installed a same-key successor or a first
                // send. Its owner retains admission; the old waiter removes no key.
                this.request_close(window, cx);
            });
        }));
        cx.notify();
        true
    }

    /// Close controls wait for accepted callbacks and the final durable snapshot.
    pub(crate) fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.persistence_closing()
            || self.persistence.native_root_close
            || self.persistence.accepted_close.is_some()
        {
            return;
        }
        if self.worktree_close.is_some() {
            return;
        }
        let worktrees = self.worktree_removals.receipts();
        if !worktrees.is_empty() {
            self.worktree_close = Some(cx.spawn_in(window, async move |this, cx| {
                let settled = crate::worktree_removals::wait(worktrees).await.is_ok();
                let _ = this.update_in(cx, |workspace, window, cx| {
                    workspace.worktree_close = None;
                    if settled {
                        workspace.request_close(window, cx);
                    } else {
                        workspace.show_worktree_recovery(window, cx);
                    }
                });
            }));
            cx.notify();
            return;
        }
        if self.native_root_actors_pending() && !self.composer_submissions_pending() {
            self.persistence.native_root_close = true;
            cx.notify();
            return;
        }
        if self.composer_close_in_progress() {
            return;
        }
        if !self.close_in_progress() && !self.composer_submissions_pending() {
            self.retry_pending_attachment_retirements(window, cx);
            if self.has_pending_attachment_retirements() {
                self.unfreeze_host_manager(cx);
                self.unfreeze_appearance(cx);
                self.unfreeze_general(cx);
                self.notify_mutation_failure("chat.nativeRecovery.storageFailed", window, cx);
                cx.notify();
                return;
            }
        }
        self.reconcile_composer_documents_for_close(window, cx);
        if self.composer_documents_pending() {
            self.authorize_submission_hydration_close();
            self.persistence.composer_hydration_close = true;
            cx.notify();
            return;
        }
        let captures = match self.freeze_composer_documents(cx) {
            Ok(captures) => captures,
            Err(_) => {
                self.unfreeze_host_manager(cx);
                self.unfreeze_appearance(cx);
                self.unfreeze_general(cx);
                self.notify_mutation_failure("chat.nativeRecovery.storageFailed", window, cx);
                cx.notify();
                return;
            }
        };
        let submissions = self.submission_close_permits();
        if !captures.is_empty() || !submissions.is_empty() {
            self.persistence.composer_close = Some(cx.spawn_in(window, async move |this, cx| {
                let mut saved = crate::composer_document::drain_submissions(&this, cx, submissions)
                    .await
                    .is_ok();
                for capture in captures {
                    if !saved {
                        break;
                    }
                    if !this
                        .update_in(cx, |workspace, _, _| {
                            workspace.composer_capture_needs_flush(&capture)
                        })
                        .unwrap_or(true)
                    {
                        continue;
                    }
                    if crate::composer_document::flush(&this, cx, capture)
                        .await
                        .is_err()
                    {
                        saved = false;
                        break;
                    }
                }
                let _ = this.update_in(cx, |this, window, cx| {
                    this.persistence.composer_close = None;
                    if saved {
                        this.request_close(window, cx);
                    } else {
                        this.unfreeze_host_manager(cx);
                        this.unfreeze_appearance(cx);
                        this.unfreeze_general(cx);
                        this.notify_mutation_failure(
                            "chat.nativeRecovery.storageFailed",
                            window,
                            cx,
                        );
                        cx.notify();
                    }
                });
            }));
            cx.notify();
            return;
        }
        if self.drain_accepted_tasks(window, cx) {
            return;
        }
        if self.effort_close.is_some() {
            return;
        }
        if let Some(pending) = self.take_effort_close() {
            self.effort_close = Some(cx.spawn_in(window, async move |this, cx| {
                let _ = pending.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.effort_close = None;
                    this.request_close(window, cx);
                });
            }));
            return;
        }
        if self.effort_preferences.has_failed_writes() {
            self.unfreeze_general(cx);
            self.notify_mutation_failure("settings.common.status.saveFailed", window, cx);
            return;
        }
        if self.persistence.settings_close.is_some() {
            return;
        }
        if let Some(settings) = &self.settings {
            settings.update(cx, |settings, cx| settings.prepare_close(window, cx));
            if let Some(pending) =
                settings.update(cx, |settings, cx| settings.take_pending_write(cx))
            {
                self.persistence.settings_close =
                    Some(cx.spawn_in(window, async move |this, cx| {
                        pending.await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.persistence.settings_close = None;
                            this.request_close(window, cx);
                        });
                    }));
                return;
            }
            if settings.read(cx).has_failed_writes(cx) {
                self.unfreeze_general(cx);
                self.notify_mutation_failure("settings.common.status.saveFailed", window, cx);
                return;
            }
        }
        if self.persistence.knowledge_close.is_some() {
            return;
        }
        let panels = self.panels.all_knowledge_views();
        let client = self
            .connection
            .as_ref()
            .and_then(|connection| connection.client.clone());
        let mut pending = Vec::new();
        for panel in &panels {
            panel.update(cx, |panel, cx| {
                if let Some(client) = &client {
                    panel.refresh_write_client(&self.runtime, client);
                }
                panel.prepare_close(window, cx);
            });
            if let Some(receipt) = panel.update(cx, |panel, cx| panel.take_pending_write(cx)) {
                pending.push(receipt);
            }
        }
        if !pending.is_empty() {
            self.persistence.knowledge_close = Some(cx.spawn_in(window, async move |this, cx| {
                for receipt in pending {
                    let _ = receipt.await;
                }
                let _ = this.update_in(cx, |this, window, cx| {
                    this.persistence.knowledge_close = None;
                    this.request_close(window, cx);
                });
            }));
            return;
        }
        if panels
            .iter()
            .any(|panel| panel.read(cx).has_failed_writes())
        {
            self.unfreeze_general(cx);
            self.notify_mutation_failure("settings.common.status.saveFailed", window, cx);
            return;
        }
        if self.general_close.is_some() {
            return;
        }
        if let Some(settings) = &self.settings {
            let general = settings.read(cx).general.clone();
            if let Some(pending) = general.update(cx, |general, cx| general.take_pending_write(cx))
            {
                self.general_close = Some(cx.spawn_in(window, async move |this, cx| {
                    let _ = pending.await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.general_close = None;
                        this.request_close(window, cx);
                    });
                }));
                return;
            }
            if general.read(cx).has_failed_writes() {
                self.unfreeze_general(cx);
                self.notify_mutation_failure("settings.common.status.saveFailed", window, cx);
                return;
            }
        }
        if self.appearance_close.is_some() {
            return;
        }
        if let Some(settings) = &self.settings {
            settings.update(cx, |settings, cx| settings.prepare_close(window, cx));
        }
        if let Some(owner) = cx.try_global::<crate::appearance_view::AppearanceOwner>() {
            let appearance = owner.0.clone();
            let pending = appearance.update(cx, |appearance, cx| appearance.take_pending_write(cx));
            if let Some(pending) = pending {
                self.appearance_close = Some(cx.spawn_in(window, async move |this, cx| {
                    pending.await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.appearance_close = None;
                        if appearance.update(cx, |appearance, _| appearance.finish_close()) {
                            this.request_close(window, cx);
                        } else {
                            this.unfreeze_general(cx);
                            this.notify_mutation_failure(
                                "settings.common.status.saveFailed",
                                window,
                                cx,
                            );
                        }
                    });
                }));
                return;
            }
            if !appearance.update(cx, |appearance, _| appearance.finish_close()) {
                self.unfreeze_general(cx);
                self.notify_mutation_failure("settings.common.status.saveFailed", window, cx);
                return;
            }
        }
        if self.host_close.is_some() {
            return;
        }
        if let Some(pending) = self.take_profile_flush(cx) {
            self.host_close = Some(cx.spawn_in(window, async move |this, cx| {
                pending.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.host_close = None;
                    let saved = this.host_manager.as_ref().is_none_or(|manager| {
                        manager.update(cx, |manager, _| manager.finish_close())
                    });
                    if saved {
                        this.request_close(window, cx);
                    } else {
                        this.unfreeze_appearance(cx);
                        this.unfreeze_general(cx);
                        this.notify_mutation_failure(
                            "desktopHostSwitcher.error.failedToSave",
                            window,
                            cx,
                        );
                        cx.notify();
                    }
                });
            }));
            return;
        }
        if self.attachment_storage_close.is_some() {
            return;
        }
        self.attachment_storage.stop_accepting();
        if let Some(drain) = self.attachment_storage.take_drain() {
            self.attachment_storage_close = Some(cx.spawn_in(window, async move |this, cx| {
                drain.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.attachment_storage_close = None;
                    this.request_close(window, cx);
                });
            }));
            return;
        }
        if self.has_uncommitted_attachment_storage() {
            self.unfreeze_host_manager(cx);
            self.unfreeze_appearance(cx);
            self.unfreeze_general(cx);
            self.notify_mutation_failure("chat.nativeRecovery.storageFailed", window, cx);
            return;
        }
        if self.is_synthetic() || !self.persistence.loaded() {
            window.remove_window();
            return;
        }
        if matches!(self.persistence.status, Status::Closing) {
            return;
        }
        self.cancel_attachment_staging(cx);
        self.attachment_retirement.cancel();
        self.persistence.debounce = None;
        let Some((stamp, saved)) = self.persistence_barrier(cx) else {
            self.unfreeze_host_manager(cx);
            self.unfreeze_appearance(cx);
            self.unfreeze_general(cx);
            return;
        };
        self.persistence.status = Status::Closing;
        self.persistence.closing = Some(cx.spawn_in(window, async move |this, cx| {
            let saved = saved.await.unwrap_or(Err(JournalFailure::WorkerStopped));
            let finished = this.update_in(cx, |this, window, cx| {
                if this.persistence.epoch != stamp.epoch {
                    return None;
                }
                // Older background receipts must not clear a failed close.
                this.persistence.last_result = stamp.revision;
                if this.has_pending_attachment_retirements()
                    || this.native_root_actors_pending()
                    || this.composer_submissions_pending()
                {
                    this.persistence.status = Status::Ready;
                    this.unfreeze_host_manager(cx);
                    this.unfreeze_appearance(cx);
                    this.unfreeze_general(cx);
                    this.record_persistence(stamp, &saved, cx);
                    this.notify_mutation_failure("chat.nativeRecovery.storageFailed", window, cx);
                    cx.notify();
                    return None;
                }
                if matches!(saved, Ok(CommitOutcome::Durable)) {
                    return this.persistence.worker.as_ref().map(JournalWorker::finish);
                }
                // Keep the worker and its exclusive lock for an explicit retry.
                // A stopped worker keeps rejecting writes; it is never reopened
                // over possibly newer data from another process.
                this.persistence.status = Status::Ready;
                this.unfreeze_host_manager(cx);
                this.unfreeze_appearance(cx);
                this.unfreeze_general(cx);
                this.record_persistence(stamp, &saved, cx);
                None
            });
            let Ok(Some(finished)) = finished else {
                return;
            };
            let finished = finished.await.unwrap_or(Err(JournalFailure::WorkerStopped));
            let _ = this.update_in(cx, |this, window, cx| {
                if this.persistence.epoch != stamp.epoch {
                    return;
                }
                if matches!(finished, Ok(CommitOutcome::Durable)) {
                    this.persistence.worker = None;
                    window.remove_window();
                } else {
                    let issue = match finished {
                        Err(JournalFailure::Invalid(_)) => StorageIssue::WriteRejected,
                        Err(error) => StorageIssue::Failure(error),
                        _ => StorageIssue::Uncertain,
                    };
                    this.persistence.status = Status::Fault {
                        loaded: true,
                        issue,
                    };
                    this.unfreeze_host_manager(cx);
                    this.unfreeze_appearance(cx);
                    this.unfreeze_general(cx);
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{AppContext, TestAppContext};

    #[gpui_kit::test]
    fn close_refuses_uncommitted_named_attachment_retirement(cx: &mut TestAppContext) {
        use crate::attachment_catalog::PreparedAttachment;
        use crate::attachment_spool::{BlobDescriptor, BlobId};
        use openchamber_native_core::ids::AttachmentId;

        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.persistence.status = Status::Ready;
                let blob = BlobId::parse("ffffffffffffffffffffffffffffffff")
                    .expect("exact unowned byte label");
                let id = AttachmentId::from(blob.as_str());
                workspace.attachments.insert(
                    id.clone(),
                    PreparedAttachment {
                        blob: BlobDescriptor::new(blob, 1, [7; 32]),
                        staging: None,
                    },
                );
                workspace.retired_attachment_blobs = (0..crate::mutation_journal::MAX_ENTRIES)
                    .map(|index| {
                        BlobId::parse(&format!("{index:032x}"))
                            .expect("bounded existing retirement label")
                    })
                    .collect();
                workspace.retire_released_native_roots(vec![id.clone()], window, cx);
                assert!(
                    workspace.has_pending_attachment_retirements(),
                    "capacity refusal retains the exact release request"
                );
                workspace.request_close(window, cx);
                assert!(workspace.has_pending_attachment_retirements());
                assert!(
                    workspace.attachments.contains_key(&id),
                    "uncommitted cleanup cannot release catalog custody"
                );
                assert!(
                    !workspace.close_in_progress(),
                    "close refusal keeps the owner usable for explicit local cleanup"
                );
            })
        })
        .expect("named pending cleanup prevents normal window removal");
        cx.update_window(window, |_, window, _| window.remove_window())
            .expect("the refused close left the test-owned window open");
    }

    #[gpui_kit::test]
    fn closing_cannot_create_settings_writers(cx: &mut TestAppContext) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.persistence.status = Status::Closing;
                workspace.open_settings(window, cx);
                workspace.show_settings(None, window, cx);
                assert!(
                    workspace.settings.is_none(),
                    "journal drain blocks new page writers"
                );
                assert!(
                    workspace.host_manager.is_none(),
                    "blocked navigation starts no host work"
                );

                workspace.persistence.status = Status::Ready;
                for phase in 0..7 {
                    let barrier = cx.background_executor().spawn(async {});
                    match phase {
                        0 => workspace.general_close = Some(barrier),
                        1 => workspace.appearance_close = Some(barrier),
                        2 => workspace.host_close = Some(barrier),
                        3 => workspace.effort_close = Some(barrier),
                        4 => workspace.persistence.composer_close = Some(barrier),
                        5 => workspace.persistence.settings_close = Some(barrier),
                        _ => workspace.persistence.knowledge_close = Some(barrier),
                    }
                    workspace.open_settings(window, cx);
                    workspace.show_settings(None, window, cx);
                    assert!(
                        workspace.settings.is_none(),
                        "an earlier close barrier also blocks new page writers"
                    );
                    assert!(
                        workspace.host_manager.is_none(),
                        "blocked navigation keeps host creation lazy"
                    );
                    workspace.general_close = None;
                    workspace.appearance_close = None;
                    workspace.host_close = None;
                    workspace.effort_close = None;
                    workspace.persistence.composer_close = None;
                    workspace.persistence.settings_close = None;
                    workspace.persistence.knowledge_close = None;
                }
                assert!(
                    !workspace.close_in_progress(),
                    "cleared barriers release navigation"
                );
            });
        })
        .expect("test window remains open");
    }

    #[gpui_kit::test]
    fn hydration_close_wait_blocks_mutations_and_cancellation_releases_it(cx: &mut TestAppContext) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.persistence.composer_hydration_close = true;
                assert!(
                    workspace.close_in_progress(),
                    "waiting for local hydration is a close barrier"
                );
                assert!(
                    !workspace.composer_editable(),
                    "typing cannot outrun a pending close"
                );
                workspace.open_settings(window, cx);
                assert!(
                    workspace.settings.is_none(),
                    "hydration close cannot create another writer"
                );
                workspace.unfreeze_general(cx);
                assert!(
                    !workspace.composer_hydration_close_in_progress(),
                    "a failed close releases the hydration gate"
                );
                assert!(
                    !workspace.close_in_progress(),
                    "independent work can resume after refusal"
                );
            });
        })
        .expect("test workspace remains open");
    }

    #[gpui_kit::test]
    fn cancelled_close_releases_effort_preference_freeze(cx: &mut TestAppContext) {
        let (_, workspace) = crate::workspace_tests::open(cx);
        cx.update(|cx| {
            workspace.update(cx, |workspace, cx| {
                assert!(
                    workspace.take_effort_close().is_none(),
                    "no accepted effort write is pending"
                );
                assert!(
                    workspace.close_in_progress(),
                    "the drain freezes further preference writes"
                );
                workspace.unfreeze_general(cx);
                assert!(
                    !workspace.effort_preferences.is_closing(),
                    "a later close failure permits explicit retry"
                );
                assert!(
                    !workspace.close_in_progress(),
                    "all close barriers were released"
                );
            });
        });
    }

    #[gpui_kit::test]
    fn obsolete_writer_receipts_cannot_clear_current_failure(cx: &mut TestAppContext) {
        let (_, workspace) = crate::workspace_tests::open(cx);
        cx.update(|cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.persistence.epoch = 2;
                workspace.persistence.status = Status::Fault {
                    loaded: true,
                    issue: StorageIssue::Failure(JournalFailure::Locked),
                };
                assert!(!workspace.record_persistence(
                    WriteStamp {
                        epoch: 1,
                        revision: 5
                    },
                    &Ok(CommitOutcome::Durable),
                    cx
                ));
                assert_eq!(workspace.persistence_notice(), Some(StorageNotice::Locked));
            })
        });
    }

    #[gpui_kit::test]
    fn older_receipts_cannot_clear_failed_close(cx: &mut TestAppContext) {
        let (_, workspace) = crate::workspace_tests::open(cx);
        cx.update(|cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.persistence.status = Status::Closing;
                workspace.record_persistence(
                    WriteStamp {
                        epoch: 0,
                        revision: 1,
                    },
                    &Ok(CommitOutcome::Durable),
                    cx,
                );
                assert!(workspace.persistence_closing());
                workspace.persistence.status = Status::Ready;
                workspace.record_persistence(
                    WriteStamp {
                        epoch: 0,
                        revision: 2,
                    },
                    &Err(JournalFailure::Io(std::io::ErrorKind::Other)),
                    cx,
                );
                workspace.record_persistence(
                    WriteStamp {
                        epoch: 0,
                        revision: 1,
                    },
                    &Ok(CommitOutcome::Durable),
                    cx,
                );
                assert_eq!(workspace.persistence_notice(), Some(StorageNotice::Io));
            });
        });
    }

    #[gpui_kit::test]
    fn hydrated_memory_cannot_reopen_a_released_writer(cx: &mut TestAppContext) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.persistence.status = Status::Fault {
                    loaded: true,
                    issue: StorageIssue::Failure(JournalFailure::WorkerStopped),
                };
                workspace.persistence.path = Some(PathBuf::from("unused-journal.json"));
                workspace.retry_persistence(window, cx);
                assert!(workspace.persistence.worker.is_none());
                assert!(matches!(
                    workspace.persistence.status,
                    Status::Fault {
                        loaded: true,
                        issue: StorageIssue::Failure(JournalFailure::WorkerStopped),
                    }
                ));
            });
        })
        .expect("window open");
    }

    #[gpui_kit::test]
    fn rejected_writes_are_distinct_from_unreadable_saved_data(cx: &mut TestAppContext) {
        let (_, workspace) = crate::workspace_tests::open(cx);
        cx.update(|cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.persistence.status = Status::Ready;
                assert!(!workspace.record_persistence(
                    WriteStamp {
                        epoch: 0,
                        revision: 1
                    },
                    &Err(JournalFailure::Invalid("text size limit exceeded")),
                    cx
                ));
                assert_eq!(
                    workspace.persistence_notice(),
                    Some(StorageNotice::WriteRejected)
                );
            })
        });
    }
}
