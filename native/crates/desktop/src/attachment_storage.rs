//! Atomic source attachment publication over serialized, per-file local spool writes.
//!
//! One source owns all normalized outputs, immutable identities and captured bytes until its
//! entire publication commits. All preparation holds commit before the first put. Successful
//! puts remain private under those holds; no DraftFile exists until every output has a durable
//! spool receipt. One checked foreground transition then publishes the whole source, followed
//! by one durable snapshot barrier before staging starts. A restart sees a complete source or
//! explicit incomplete holds, never a partial document and its dangling image citations.
//!
//! Failed sources retain their original bytes, identities and successful put receipts. Explicit
//! retry skips completed puts and never rereads a selected path. Cancellation revokes the exact
//! source's publication authority, consumes accepted I/O receipts, then retires its exact holds.
//! Published cleanup uses the parent's named-resource retirement boundary and its live roots.
//!
//! Source paths exist only in the ephemeral input queue/background intake. Default replays no
//! work. Normal close stops admission and awaits take_drain while foreground updates can run,
//! then checks has_uncommitted. GPUI's blocked quit observer cannot drain this coordinator.

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use gpui_kit::{AsyncWindowContext, Context, Task, WeakEntity, Window};
use openchamber_native_core::draft::{Draft, DraftKey};
use openchamber_native_core::draft_content::{DraftFile, DraftFileContent};
use openchamber_native_core::ids::AttachmentId;

use crate::attachment_catalog::{PreparationHold, PreparedAttachment};
use crate::attachment_files::{MAX_INLINE_ATTACHMENT_BYTES, NormalizedAttachmentFile};
use crate::attachment_intake::{
    self, AttachmentInputSource, AttachmentIntake, AttachmentIntakeError,
};
use crate::attachment_spool::{BlobDescriptor, BlobId, SpoolCommit};
use crate::journal_worker::{BlobFailure, CommitOutcome, JournalFailure, JournalReceipt};
use crate::locale;
use crate::mutation_journal::{MAX_DIRECTORY, MAX_ENTRIES, MAX_ID};
use crate::state_persistence::WriteStamp;
use crate::workspace::Workspace;

const MAX_QUEUED_BYTES: usize = 2 * MAX_INLINE_ATTACHMENT_BYTES;
// document_attachments.rs admits 40 MiB of cited image bytes and 500,000 UTF-16 text units.
// A UTF-16 unit needs at most 3 UTF-8 bytes. Intake compacts Vec capacities off the UI thread.
const MAX_NORMALIZED_SOURCE_BYTES: usize = 40 * 1024 * 1024 + 500_000 * 3;
// Reserve room for a failed maximum source and one independent maximum source.
const MAX_RECOVERY_BYTES: usize = 2 * MAX_NORMALIZED_SOURCE_BYTES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttachmentStorageFailure {
    Unavailable,
    Capacity,
    IdentityExhausted,
    Intake(AttachmentIntakeError),
    Persistence,
    Put(BlobFailure),
    SpoolUncertain,
    Referenced,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttachmentStorageStatus {
    Queued,
    Preparing,
    Saving,
    Retiring,
    Failed(AttachmentStorageFailure),
}

/// Source display metadata. This row grants no send authority and is never a draft placeholder.
#[derive(Clone)]
pub(crate) struct AttachmentStorageRow {
    pub ticket: u64,
    pub owner: DraftKey,
    pub filename: String,
    /// The source's first real output ID. Any member ID resolves to this same source for actions.
    pub id: Option<AttachmentId>,
    pub source_document_id: Option<AttachmentId>,
    pub status: AttachmentStorageStatus,
}

impl fmt::Debug for AttachmentStorageRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachmentStorageRow")
            .field("ticket", &self.ticket)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(crate) struct RejectedAttachmentInput {
    pub input: AttachmentIntake,
}

struct QueuedInput {
    ticket: u64,
    owner: DraftKey,
    input: AttachmentIntake,
}

enum StorageWork {
    Input(QueuedInput),
    Retry(AttachmentId),
    Retire(AttachmentId),
}

struct ActiveSource {
    ticket: u64,
    owner: DraftKey,
    key: Option<AttachmentId>,
    group: Option<AttachmentId>,
    cancelled: bool,
}

struct CapturedMember {
    hold: PreparationHold,
    file: NormalizedAttachmentFile,
    descriptor: Option<BlobDescriptor>,
}

impl CapturedMember {
    fn metadata(&self, group: &Option<AttachmentId>) -> DraftFile {
        DraftFile {
            id: self.hold.id.clone(),
            name: Some(self.file.filename.clone()),
            media_type: Some(self.file.media_type.to_owned()),
            byte_len: Some(self.file.content.len() as u64),
            source_document_id: group.clone(),
            content: DraftFileContent::Prepared,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourcePhase {
    Proposed,
    Published,
}

struct CapturedSource {
    ticket: u64,
    owner: DraftKey,
    key: AttachmentId,
    group: Option<AttachmentId>,
    members: Vec<CapturedMember>,
    capacity: usize,
    phase: SourcePhase,
    cancelled: bool,
}

#[derive(Clone, Copy)]
enum RetirementPhase {
    Intent,
    Release,
    Clear,
}

struct Retirement {
    hold: PreparationHold,
    phase: RetirementPhase,
    ticket: u64,
    /// A source cleanup owns its shared row until every member finishes.
    standalone: bool,
}

#[derive(Default)]
pub(crate) struct AttachmentStorage {
    queue: VecDeque<StorageWork>,
    queued_bytes: usize,
    queued_metadata: usize,
    rows: BTreeMap<u64, AttachmentStorageRow>,
    active: Option<ActiveSource>,
    task: Option<Task<()>>,
    running: bool,
    stopped: bool,
    sequence: u64,
    recovery_bytes: usize,
    sources: HashMap<AttachmentId, CapturedSource>,
    /// Remains present while the coordinator owns a source outside sources.
    members: HashMap<AttachmentId, AttachmentId>,
    holds: HashMap<AttachmentId, PreparationHold>,
    retirements: HashMap<AttachmentId, Retirement>,
    #[cfg(test)]
    gates: TestGates,
}

impl fmt::Debug for AttachmentStorage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachmentStorage")
            .field("queued", &self.queue.len())
            .field("queued_bytes", &self.queued_bytes)
            .field("recovery_bytes", &self.recovery_bytes)
            .field("running", &self.running)
            .finish_non_exhaustive()
    }
}

impl Drop for AttachmentStorage {
    fn drop(&mut self) {
        // Unexpected entity drop must not discard a waiter for accepted I/O. Durable holds
        // protect unpublished blobs if the weak entity can no longer publish the outcome.
        if let Some(task) = self.task.take() {
            task.detach();
        }
    }
}

impl AttachmentStorage {
    #[cfg(test)]
    pub(crate) fn is_busy(&self) -> bool {
        self.running || !self.queue.is_empty()
    }
    pub(crate) fn has_uncommitted(&self) -> bool {
        !self.sources.is_empty() || !self.holds.is_empty() || !self.retirements.is_empty()
    }
    /// Shared retirement must leave this owner's exact intent until its clear phase finishes.
    pub(crate) fn owns_retirement(&self, blob: &BlobId) -> bool {
        self.retirements
            .values()
            .any(|retirement| &retirement.hold.blob == blob)
    }
    pub(crate) fn stop_accepting(&mut self) {
        self.stopped = true;
    }
    pub(crate) fn resume_accepting(&mut self) {
        self.stopped = false;
    }
    pub(crate) fn take_drain(&mut self) -> Option<Task<()>> {
        self.task.take()
    }
    pub(crate) fn rows(&self, owner: &DraftKey) -> impl Iterator<Item = &AttachmentStorageRow> {
        self.rows.values().filter(move |row| &row.owner == owner)
    }
    pub(crate) fn has_pending(&self, owner: &DraftKey) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| &active.owner == owner && !active.cancelled)
            || self.queue.iter().any(|work| match work {
                StorageWork::Input(input) => &input.owner == owner,
                StorageWork::Retry(key) => self
                    .sources
                    .get(key)
                    .is_some_and(|source| &source.owner == owner && !source.cancelled),
                StorageWork::Retire(_) => false,
            })
    }
    fn next_sequence(&mut self) -> Result<u64, AttachmentStorageFailure> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(AttachmentStorageFailure::IdentityExhausted)?;
        Ok(self.sequence)
    }
    fn source_key(&self, id: &AttachmentId) -> Option<AttachmentId> {
        self.members
            .get(id)
            .cloned()
            .or_else(|| self.sources.contains_key(id).then(|| id.clone()))
    }
    fn current(&self, source: &CapturedSource) -> bool {
        !source.cancelled
            && self.active.as_ref().is_some_and(|active| {
                active.ticket == source.ticket
                    && active.owner == source.owner
                    && active.key.as_ref() == Some(&source.key)
                    && !active.cancelled
            })
    }
    fn current_input(&self, ticket: u64) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.ticket == ticket && !active.cancelled)
    }
    fn owns_work(&self, key: &AttachmentId) -> bool {
        self.active.as_ref().is_some_and(|active| active.key.as_ref() == Some(key))
            || self.queue.iter().any(|work| matches!(work, StorageWork::Retry(id) | StorageWork::Retire(id) if id == key))
    }
    fn fail(&mut self, ticket: u64, failure: AttachmentStorageFailure) {
        if let Some(row) = self.rows.get_mut(&ticket) {
            row.status = AttachmentStorageStatus::Failed(failure);
        }
    }
    fn cancel_source(&mut self, key: &AttachmentId) {
        if let Some(active) = &mut self.active
            && active.key.as_ref() == Some(key)
        {
            active.cancelled = true;
        }
        if let Some(source) = self.sources.get_mut(key) {
            source.cancelled = true;
        }
        for row in self
            .rows
            .values_mut()
            .filter(|row| row.id.as_ref() == Some(key))
        {
            row.status = AttachmentStorageStatus::Retiring;
        }
    }
    fn cancel_ticket(&mut self, ticket: u64) {
        if let Some(active) = &mut self.active
            && active.ticket == ticket
        {
            active.cancelled = true;
        }
        let mut removed = None;
        self.queue.retain(|work| {
            if let StorageWork::Input(input) = work
                && input.ticket == ticket
            {
                removed = Some((input_bytes(&input.input), metadata_entries(&input.input)));
                false
            } else {
                true
            }
        });
        if let Some((bytes, metadata)) = removed {
            self.queued_bytes -= bytes;
            self.queued_metadata -= metadata;
            self.rows.remove(&ticket);
        } else if self.rows.get(&ticket).is_some_and(|row| {
            matches!(row.status, AttachmentStorageStatus::Failed(_)) && row.id.is_none()
        }) {
            self.rows.remove(&ticket);
        } else if let Some(row) = self.rows.get_mut(&ticket) {
            row.status = AttachmentStorageStatus::Retiring;
        }
    }
    /// Queues the input and answers its row's ticket.
    fn admit(
        &mut self,
        owner: DraftKey,
        input: AttachmentIntake,
    ) -> Result<u64, Box<RejectedAttachmentInput>> {
        let bytes = input_bytes(&input);
        if self.stopped
            || matches!(&input.source,
                AttachmentInputSource::Bytes(bytes) | AttachmentInputSource::ClipboardImage { bytes, .. }
                    if bytes.len() > MAX_INLINE_ATTACHMENT_BYTES)
            || self.rows.len() >= MAX_ENTRIES
            || self.queue.len() >= MAX_ENTRIES
            || bytes > MAX_QUEUED_BYTES.saturating_sub(self.queued_bytes)
            || metadata_entries(&input) > MAX_ENTRIES.saturating_sub(self.queued_metadata)
            || !bounded_metadata(&input)
        {
            return Err(Box::new(RejectedAttachmentInput { input }));
        }
        let Ok(ticket) = self.next_sequence() else {
            return Err(Box::new(RejectedAttachmentInput { input }));
        };
        self.rows.insert(
            ticket,
            AttachmentStorageRow {
                ticket,
                owner: owner.clone(),
                filename: input.filename.clone(),
                id: None,
                source_document_id: None,
                status: AttachmentStorageStatus::Queued,
            },
        );
        self.queued_bytes += bytes;
        self.queued_metadata += metadata_entries(&input);
        self.queue.push_back(StorageWork::Input(QueuedInput {
            ticket,
            owner,
            input,
        }));
        Ok(ticket)
    }
    fn forget_source(&mut self, source: &CapturedSource) {
        self.recovery_bytes -= source.capacity;
        self.rows.remove(&source.ticket);
        for member in &source.members {
            self.members.remove(&member.hold.id);
            self.holds.remove(&member.hold.id);
        }
    }
}

fn input_bytes(input: &AttachmentIntake) -> usize {
    match &input.source {
        AttachmentInputSource::Bytes(bytes)
        | AttachmentInputSource::ClipboardImage { bytes, .. } => bytes.capacity(),
        AttachmentInputSource::LocalPath(_) => 0,
        #[cfg(test)]
        AttachmentInputSource::LocalFile(_) => 0,
    }
}
fn metadata_entries(input: &AttachmentIntake) -> usize {
    input.reserved_filenames.len().saturating_add(1)
}
fn bounded_metadata(input: &AttachmentIntake) -> bool {
    input.filename.len() <= MAX_ID
        && input
            .declared_media_type
            .as_ref()
            .is_none_or(|mime| mime.len() <= MAX_ID)
        && input
            .reserved_filenames
            .iter()
            .all(|name| name.len() <= MAX_ID)
        && match &input.source {
            AttachmentInputSource::LocalPath(path) => path.as_os_str().len() <= MAX_DIRECTORY,
            AttachmentInputSource::Bytes(_) | AttachmentInputSource::ClipboardImage { .. } => true,
            #[cfg(test)]
            AttachmentInputSource::LocalFile(_) => true,
        }
}

impl Workspace {
    pub(crate) fn ingest_attachments(
        &mut self,
        owner: DraftKey,
        inputs: Vec<AttachmentIntake>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<RejectedAttachmentInput> {
        let mut rejected = Vec::new();
        for input in inputs {
            if let Err(input) = self.admit_attachment(&owner, input, window, cx) {
                rejected.push(*input);
            }
        }
        self.drive_attachment_storage(window, cx);
        cx.notify();
        rejected
    }

    /// Admits one input and answers its row's ticket, which names the row
    /// until it publishes, fails or is removed. A refusal is reported and
    /// answers `None`.
    pub(crate) fn ingest_attachment(
        &mut self,
        owner: DraftKey,
        input: AttachmentIntake,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let ticket = self.admit_attachment(&owner, input, window, cx).ok();
        self.drive_attachment_storage(window, cx);
        cx.notify();
        ticket
    }

    fn admit_attachment(
        &mut self,
        owner: &DraftKey,
        input: AttachmentIntake,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<u64, Box<RejectedAttachmentInput>> {
        let result = if !self.persistence_loaded()
            || self.close_in_progress()
            || self.attachment_worker().is_none()
        {
            Err(Box::new(RejectedAttachmentInput { input }))
        } else {
            self.attachment_storage.admit(owner.clone(), input)
        };
        if let Err(input) = &result {
            self.storage_notice(owner, &input.input.filename, window, cx);
        }
        result
    }

    /// Cancels exactly one source row, including a capture assigned while its action was queued.
    pub(crate) fn cancel_attachment_input(
        &mut self,
        ticket: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A row removed before it published takes its citation with it.
        self.retract_citation(ticket, window, cx);
        if let Some(id) = self
            .attachment_storage
            .rows
            .get(&ticket)
            .and_then(|row| row.id.clone())
        {
            self.discard_attachment_capture(&id, window, cx);
            return;
        }
        self.attachment_storage.cancel_ticket(ticket);
        cx.notify();
    }
    pub(crate) fn cancel_attachment_inputs(&mut self, owner: &DraftKey) {
        let tickets: Vec<_> = self
            .attachment_storage
            .rows
            .values()
            .filter(|row| &row.owner == owner && row.id.is_none())
            .map(|row| row.ticket)
            .collect();
        for ticket in tickets {
            self.attachment_storage.cancel_ticket(ticket);
        }
        if let Some(active) = &mut self.attachment_storage.active
            && &active.owner == owner
        {
            active.cancelled = true;
        }
        for source in self
            .attachment_storage
            .sources
            .values_mut()
            .filter(|source| &source.owner == owner)
        {
            source.cancelled = true;
        }
    }
    pub(crate) fn cancel_attachment_group(
        &mut self,
        owner: &DraftKey,
        group: &AttachmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = &mut self.attachment_storage.active
            && &active.owner == owner
            && active.group.as_ref() == Some(group)
        {
            active.cancelled = true;
        }
        let keys: Vec<_> = self
            .attachment_storage
            .sources
            .values()
            .filter(|source| &source.owner == owner && source.group.as_ref() == Some(group))
            .map(|source| source.key.clone())
            .collect();
        for key in keys {
            self.discard_attachment_capture(&key, window, cx);
        }
        cx.notify();
    }
    pub(crate) fn retry_attachment_storage(
        &mut self,
        id: &AttachmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.attachment_storage.source_key(id) else {
            if self.attachment_storage.retirements.contains_key(id) {
                self.retire_preparation(id, window, cx);
            }
            return;
        };
        if !self.persistence_loaded()
            || self.close_in_progress()
            || self.attachment_storage.stopped
            || self.attachment_storage.owns_work(&key)
            || self.attachment_storage.queue.len() >= MAX_ENTRIES
        {
            return;
        }
        self.attachment_storage
            .queue
            .push_back(StorageWork::Retry(key));
        self.drive_attachment_storage(window, cx);
        cx.notify();
    }
    pub(crate) fn discard_attachment_capture(
        &mut self,
        id: &AttachmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.attachment_storage.source_key(id) else {
            self.retire_preparation(id, window, cx);
            return;
        };
        // Both an active source and a queued retry are revoked before the work-admission guard.
        // Their coordinator retains every accepted receipt and consumes this cancellation.
        self.attachment_storage.cancel_source(&key);
        cx.notify();
        if self.attachment_storage.owns_work(&key) || self.close_in_progress() {
            return;
        }
        self.attachment_storage
            .queue
            .push_back(StorageWork::Retry(key));
        self.drive_attachment_storage(window, cx);
    }
    pub(crate) fn has_uncommitted_attachment_storage(&self) -> bool {
        self.attachment_storage.has_uncommitted() || !self.attachment_preparations.is_empty()
    }

    /// Explicit recovery of an incomplete startup hold, without scanning or reading its blob.
    pub(crate) fn retire_preparation(
        &mut self,
        id: &AttachmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_in_progress()
            || !self.persistence_loaded()
            || self.attachment_storage.members.contains_key(id)
            || self.attachment_storage.owns_work(id)
            || self.attachment_storage.queue.len() >= MAX_ENTRIES
        {
            return;
        }
        if !self.attachment_storage.retirements.contains_key(id) {
            let Some(hold) = self
                .attachment_preparations
                .iter()
                .find(|hold| &hold.id == id)
                .cloned()
            else {
                return;
            };
            if self.storage_blob_referenced(&hold.id, &hold.blob) {
                return;
            }
            let Ok(ticket) = self.attachment_storage.next_sequence() else {
                return;
            };
            if self.attachment_storage.rows.len() >= MAX_ENTRIES {
                return;
            }
            self.attachment_storage.rows.insert(
                ticket,
                AttachmentStorageRow {
                    ticket,
                    owner: hold.owner.clone(),
                    filename: String::new(),
                    id: Some(id.clone()),
                    source_document_id: None,
                    status: AttachmentStorageStatus::Retiring,
                },
            );
            self.attachment_storage
                .holds
                .insert(id.clone(), hold.clone());
            self.attachment_storage.retirements.insert(
                id.clone(),
                Retirement {
                    hold,
                    ticket,
                    phase: RetirementPhase::Intent,
                    standalone: true,
                },
            );
        }
        self.attachment_storage
            .queue
            .push_back(StorageWork::Retire(id.clone()));
        self.drive_attachment_storage(window, cx);
        cx.notify();
    }

    pub(crate) fn drive_attachment_storage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.attachment_storage.running || self.attachment_storage.queue.is_empty() {
            return;
        }
        self.attachment_storage.running = true;
        self.attachment_storage.task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let work = this.update_in(cx, |this, _, cx| {
                    let work = this.attachment_storage.queue.pop_front();
                    if work.is_none() {
                        this.attachment_storage.active = None;
                        this.attachment_storage.running = false;
                        this.attachment_storage.task = None;
                    }
                    cx.notify();
                    work
                });
                let Ok(Some(work)) = work else { return };
                match work {
                    StorageWork::Input(input) => process_input(&this, cx, input).await,
                    StorageWork::Retry(key) => {
                        let source = this
                            .update_in(cx, |this, _, cx| {
                                let source = this.attachment_storage.sources.remove(&key)?;
                                this.attachment_storage.active = Some(ActiveSource {
                                    ticket: source.ticket,
                                    owner: source.owner.clone(),
                                    key: Some(key),
                                    group: source.group.clone(),
                                    cancelled: source.cancelled,
                                });
                                if let Some(row) =
                                    this.attachment_storage.rows.get_mut(&source.ticket)
                                {
                                    row.status = if source.cancelled {
                                        AttachmentStorageStatus::Retiring
                                    } else {
                                        AttachmentStorageStatus::Saving
                                    };
                                }
                                cx.notify();
                                Some(source)
                            })
                            .ok()
                            .flatten();
                        if let Some(source) = source {
                            process_source(&this, cx, source).await;
                        }
                    }
                    StorageWork::Retire(id) => {
                        if !retire_hold(&this, cx, id.clone()).await {
                            let _ = this.update_in(cx, |this, window, cx| {
                                if let Some(retirement) =
                                    this.attachment_storage.retirements.get(&id)
                                {
                                    let ticket = retirement.ticket;
                                    let owner = retirement.hold.owner.clone();
                                    this.attachment_storage
                                        .fail(ticket, AttachmentStorageFailure::Persistence);
                                    this.storage_notice(&owner, "", window, cx);
                                    cx.notify();
                                }
                            });
                        }
                    }
                }
                let _ = this.update_in(cx, |this, _, cx| {
                    this.attachment_storage.active = None;
                    cx.notify();
                });
            }
        }));
    }

    fn storage_notice(
        &self,
        owner: &DraftKey,
        filename: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if owner.runtime() != &self.runtime {
            return;
        }
        let text = if filename.is_empty() || filename.len() > 255 {
            locale::strings(cx).get("chat.chatInput.toast.attachFileFailed")
        } else {
            locale::strings(cx).format(
                "chat.chatInput.toast.attachNamedFailed",
                &[("name", filename)],
            )
        };
        window.push_notification(Notification::error(text), cx);
    }
    fn storage_blob_referenced(&self, id: &AttachmentId, blob: &BlobId) -> bool {
        self.attachments
            .iter()
            .any(|(key, prepared)| key == id || prepared.blob.id() == blob)
            || self.queue_storage.contains_blob(blob)
            || self.storage_id_referenced(id)
    }
    fn storage_id_referenced(&self, id: &AttachmentId) -> bool {
        self.queue_storage.references_native(id)
            || self.queue.enqueues.references_native(id)
            || self.queue.prefixes.references_native(id)
            || self.queue.local_sends.references_native(id)
            || self
                .drafts
                .iter()
                .any(|(_, draft)| draft_references(draft, id))
            || self
                .send_attempts
                .iter()
                .any(|(_, send)| send.resources.contains(id) || draft_references(&send.draft, id))
            || self.creations.iter().any(|(_, create)| {
                create.resources.contains(id) || draft_references(&create.draft, id)
            })
            || self
                .starters
                .captures
                .iter()
                .any(|starter| draft_references(&starter.draft, id))
    }
    fn storage_id_in_use(&self, id: &AttachmentId) -> bool {
        self.attachments.contains_key(id)
            || BlobId::parse(id.as_str()).is_ok_and(|blob| self.queue_storage.contains_blob(&blob))
            || self.attachment_storage.members.contains_key(id)
            || self.attachment_storage.holds.contains_key(id)
            || self
                .attachment_preparations
                .iter()
                .any(|hold| &hold.id == id)
            || self
                .retired_attachment_blobs
                .iter()
                .any(|blob| blob.as_str() == id.as_str())
            || self
                .attachment_storage
                .active
                .as_ref()
                .is_some_and(|active| active.group.as_ref() == Some(id))
            || self
                .attachment_storage
                .sources
                .values()
                .any(|source| source.group.as_ref() == Some(id))
            || self.storage_id_referenced(id)
    }
    pub(crate) fn new_storage_id(&mut self) -> Result<AttachmentId, AttachmentStorageFailure> {
        for _ in 0..16 {
            let sequence = self.attachment_storage.next_sequence()?;
            let id = AttachmentId::from(random_label(sequence));
            if !self.storage_id_in_use(&id) {
                return Ok(id);
            }
        }
        Err(AttachmentStorageFailure::IdentityExhausted)
    }

    fn capture_normalized_source(
        &mut self,
        ticket: u64,
        owner: DraftKey,
        files: Vec<NormalizedAttachmentFile>,
    ) -> Result<CapturedSource, AttachmentStorageFailure> {
        let capacity = files
            .iter()
            .try_fold(0usize, |total, file| {
                total.checked_add(file.content.capacity())
            })
            .ok_or(AttachmentStorageFailure::Capacity)?;
        if files.is_empty() {
            return Err(AttachmentStorageFailure::Intake(
                AttachmentIntakeError::Unsupported,
            ));
        }
        if self
            .attachment_preparations
            .len()
            .saturating_add(files.len())
            > MAX_ENTRIES
            || capacity > MAX_NORMALIZED_SOURCE_BYTES
            || capacity > MAX_RECOVERY_BYTES.saturating_sub(self.attachment_storage.recovery_bytes)
        {
            return Err(AttachmentStorageFailure::Capacity);
        }
        let group = if files.len() > 1 {
            Some(self.new_storage_id()?)
        } else {
            None
        };
        let mut members: Vec<CapturedMember> = Vec::with_capacity(files.len());
        // No put occurs until this entire vector and its recovery reservation have been accepted.
        for file in files {
            let mut proposed = None;
            for _ in 0..16 {
                let id = self.new_storage_id()?;
                if group.as_ref() != Some(&id) && members.iter().all(|member| member.hold.id != id)
                {
                    proposed = Some(id);
                    break;
                }
            }
            let id = proposed.ok_or(AttachmentStorageFailure::IdentityExhausted)?;
            let blob = BlobId::parse(id.as_str()).expect("generated 32-digit lowercase hex label");
            let hold = PreparationHold {
                owner: owner.clone(),
                id,
                generation: ticket,
                blob,
            };
            members.push(CapturedMember {
                hold,
                file,
                descriptor: None,
            });
        }
        let holds: Vec<_> = members.iter().map(|member| member.hold.clone()).collect();
        self.validate_attachment_preparation_admission(&holds)
            .map_err(|_| AttachmentStorageFailure::Capacity)?;
        let key = members[0].hold.id.clone();
        for member in &members {
            self.attachment_storage
                .holds
                .insert(member.hold.id.clone(), member.hold.clone());
            self.attachment_preparations.push(member.hold.clone());
            self.attachment_storage
                .members
                .insert(member.hold.id.clone(), key.clone());
        }
        self.attachment_storage.recovery_bytes += capacity;
        if let Some(active) = &mut self.attachment_storage.active {
            active.key = Some(key.clone());
            active.group = group.clone();
        }
        if let Some(row) = self.attachment_storage.rows.get_mut(&ticket) {
            row.id = Some(key.clone());
            row.source_document_id = group.clone();
            row.status = AttachmentStorageStatus::Saving;
        }
        Ok(CapturedSource {
            ticket,
            owner,
            key,
            group,
            members,
            capacity,
            phase: SourcePhase::Proposed,
            cancelled: false,
        })
    }

    /// All checks precede catalog, draft-content and hold mutations. Metadata is small and owned;
    /// the existing DraftContent is moved out only after the parent's canonical preflight passes.
    fn publish_storage_source(
        &mut self,
        source: &CapturedSource,
        cx: &mut Context<Self>,
    ) -> Result<(), AttachmentStorageFailure> {
        if !self.attachment_storage.current(source) {
            return Err(AttachmentStorageFailure::Unavailable);
        }
        if !self.storage_publication_counts_fit(source) {
            return Err(AttachmentStorageFailure::Capacity);
        }
        self.save_composer_draft(cx);
        if !self.storage_publication_counts_fit(source) {
            return Err(AttachmentStorageFailure::Capacity);
        }
        let publication: Vec<_> = source
            .members
            .iter()
            .map(|member| {
                member
                    .descriptor
                    .map(|descriptor| (&member.hold, descriptor, member.metadata(&source.group)))
                    .ok_or(AttachmentStorageFailure::Unavailable)
            })
            .collect::<Result<_, _>>()?;
        self.validate_attachment_bundle_publication(&source.owner, &publication)
            .map_err(|_| AttachmentStorageFailure::Capacity)?;
        let mut draft = self.drafts.take(&source.owner).unwrap_or_default();
        for (hold, descriptor, file) in publication {
            self.attachments.insert(
                hold.id.clone(),
                PreparedAttachment {
                    blob: descriptor,
                    staging: None,
                },
            );
            draft.content.files.push(file);
        }
        let holds: HashMap<_, _> = source
            .members
            .iter()
            .map(|member| (&member.hold.id, &member.hold))
            .collect();
        self.attachment_preparations.retain(|hold| {
            holds
                .get(&hold.id)
                .is_none_or(|expected| **expected != *hold)
        });
        self.save_draft_preview(&source.owner, &draft, cx);
        self.drafts.save(source.owner.clone(), draft);
        cx.notify();
        Ok(())
    }

    fn storage_publication_counts_fit(&self, source: &CapturedSource) -> bool {
        !(self.attachments.len().saturating_add(source.members.len()) > MAX_ENTRIES
            || (self.drafts.get(&source.owner).is_none() && self.drafts.len() >= MAX_ENTRIES)
            || source
                .members
                .iter()
                .any(|member| self.attachments.contains_key(&member.hold.id)))
    }

    fn storage_source_owned(&self, source: &CapturedSource) -> bool {
        self.drafts.get(&source.owner).is_some_and(|draft| {
            source.members.iter().all(|member| {
                draft
                    .content
                    .files
                    .contains(&member.metadata(&source.group))
            })
        })
    }
}

fn draft_references(draft: &Draft, id: &AttachmentId) -> bool {
    draft
        .content
        .files
        .iter()
        .any(|file| &file.id == id || file.source_document_id.as_ref() == Some(id))
}
fn random_label(sequence: u64) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut halves = [0; 2];
    for (index, half) in halves.iter_mut().enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(sequence);
        hasher.write_usize(index);
        *half = hasher.finish();
    }
    format!("{:016x}{:016x}", halves[0], halves[1])
}

async fn process_input(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    mut input: QueuedInput,
) {
    let begun = this.update_in(cx, |this, _, cx| {
        this.attachment_storage.queued_bytes -= input_bytes(&input.input);
        this.attachment_storage.queued_metadata -= metadata_entries(&input.input);
        this.attachment_storage.active = Some(ActiveSource {
            ticket: input.ticket,
            owner: input.owner.clone(),
            key: None,
            group: None,
            cancelled: false,
        });
        if let Some(row) = this.attachment_storage.rows.get_mut(&input.ticket) {
            row.status = AttachmentStorageStatus::Preparing;
        }
        if let Some(draft) = this.drafts.get(&input.owner) {
            input.input.reserved_filenames.extend(
                draft
                    .content
                    .files
                    .iter()
                    .filter_map(|file| file.name.clone()),
            );
        }
        for source in this
            .attachment_storage
            .sources
            .values()
            .filter(|source| source.owner == input.owner)
        {
            input.input.reserved_filenames.extend(
                source
                    .members
                    .iter()
                    .map(|member| member.file.filename.clone()),
            );
        }
        cx.notify();
    });
    if begun.is_err() {
        return;
    }
    let ticket = input.ticket;
    let owner = input.owner;
    let prepared = cx
        .background_executor()
        .spawn(async move {
            let mut files = attachment_intake::prepare_attachment(input.input)?;
            for file in &mut files {
                file.content = std::mem::take(&mut file.content)
                    .into_boxed_slice()
                    .into_vec();
            }
            Ok::<_, AttachmentIntakeError>(files)
        })
        .await;
    #[cfg(test)]
    test_gate(this, cx, TestBoundary::Prepared).await;
    let source = this
        .update_in(cx, |this, window, cx| {
            if !this.attachment_storage.current_input(ticket) {
                this.attachment_storage.rows.remove(&ticket);
                this.retract_citation(ticket, window, cx);
                cx.notify();
                return None;
            }
            this.save_composer_draft(cx);
            let result = prepared
                .map_err(AttachmentStorageFailure::Intake)
                .and_then(|files| this.capture_normalized_source(ticket, owner.clone(), files));
            match result {
                Ok(source) => Some(source),
                Err(error) => {
                    this.attachment_storage.fail(ticket, error);
                    if let Some(row) = this.attachment_storage.rows.get(&ticket) {
                        this.storage_notice(&owner, &row.filename, window, cx);
                    }
                    // A folder, an unreadable or unsupported file, a refused
                    // name: nothing attached, so nothing stays cited.
                    this.retract_citation(ticket, window, cx);
                    cx.notify();
                    None
                }
            }
        })
        .ok()
        .flatten();
    if let Some(source) = source {
        process_source(this, cx, source).await;
    }
}

async fn durable_barrier(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    barrier: Option<(WriteStamp, JournalReceipt)>,
) -> bool {
    let Some((stamp, receipt)) = barrier else {
        return false;
    };
    let result = receipt.await.unwrap_or(Err(JournalFailure::WorkerStopped));
    this.update_in(cx, |this, _, cx| {
        this.record_persistence(stamp, &result, cx)
    })
    .unwrap_or(false)
        && matches!(result, Ok(CommitOutcome::Durable))
}

async fn process_source(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    mut source: CapturedSource,
) {
    if !is_current(this, cx, &source) {
        source.cancelled = true;
        cancel_source_after_receipt(this, cx, source).await;
        return;
    }
    if source.phase == SourcePhase::Proposed {
        let barrier = this
            .update_in(cx, |this, _, cx| {
                if !this.attachment_storage.current(&source)
                    || source.members.iter().any(|member| {
                        this.attachment_preparations
                            .iter()
                            .find(|hold| hold.id == member.hold.id)
                            != Some(&member.hold)
                    })
                {
                    return None;
                }
                this.persistence_barrier(cx)
            })
            .ok()
            .flatten();
        if !durable_barrier(this, cx, barrier).await {
            finish_failed_source(this, cx, source, AttachmentStorageFailure::Persistence).await;
            return;
        }
        for index in 0..source.members.len() {
            if source.members[index].descriptor.is_some() {
                continue;
            }
            if !is_current(this, cx, &source) {
                source.cancelled = true;
                cancel_source_after_receipt(this, cx, source).await;
                return;
            }
            let (returned, bytes) = cx
                .background_executor()
                .spawn(async move {
                    let bytes = source.members[index].file.content.clone();
                    (source, bytes)
                })
                .await;
            source = returned;
            let put = this
                .update_in(cx, |this, _, _| {
                    if !this.attachment_storage.current(&source) {
                        return None;
                    }
                    let hold = &source.members[index].hold;
                    this.attachment_worker().map(|worker| {
                        worker.put_attachment(
                            hold.owner.clone(),
                            hold.id.clone(),
                            hold.generation,
                            hold.blob,
                            bytes,
                        )
                    })
                })
                .ok()
                .flatten();
            let Some(put) = put else {
                finish_failed_source(this, cx, source, AttachmentStorageFailure::Unavailable).await;
                return;
            };
            #[cfg(test)]
            test_gate(this, cx, TestBoundary::PutAccepted).await;
            // Cancellation never drops this accepted receipt, even if the selected owner changes.
            let receipt = put
                .await
                .unwrap_or(Err(BlobFailure::Journal(JournalFailure::WorkerStopped)));
            match receipt {
                Ok(receipt)
                    if receipt.commit == SpoolCommit::Durable
                        && receipt.descriptor.id() == &source.members[index].hold.blob
                        && receipt.descriptor.byte_len()
                            == source.members[index].file.content.len() as u64 =>
                {
                    source.members[index].descriptor = Some(receipt.descriptor);
                }
                Ok(_) => {
                    finish_failed_source(
                        this,
                        cx,
                        source,
                        AttachmentStorageFailure::SpoolUncertain,
                    )
                    .await;
                    return;
                }
                Err(error) => {
                    finish_failed_source(this, cx, source, AttachmentStorageFailure::Put(error))
                        .await;
                    return;
                }
            }
        }
        let result = this
            .update_in(cx, |this, _, cx| {
                let result = this.publish_storage_source(&source, cx);
                if result.is_ok() {
                    // The files are in the draft: the citation stays.
                    this.attachment_input.settle_citation(source.ticket);
                }
                result
            })
            .unwrap_or(Err(AttachmentStorageFailure::Unavailable));
        if let Err(error) = result {
            finish_failed_source(this, cx, source, error).await;
            return;
        }
        source.phase = SourcePhase::Published;
        #[cfg(test)]
        test_gate(this, cx, TestBoundary::Published).await;
    }
    if !is_current(this, cx, &source) {
        source.cancelled = true;
        cancel_source_after_receipt(this, cx, source).await;
        return;
    }
    let known = this
        .update_in(cx, |this, _, _| {
            source.members.iter().all(|member| {
                this.attachments
                    .get(&member.hold.id)
                    .is_some_and(|prepared| Some(prepared.blob) == member.descriptor)
            })
        })
        .unwrap_or(false);
    if !known {
        finish_failed_source(this, cx, source, AttachmentStorageFailure::Unavailable).await;
        return;
    }
    let barrier = this
        .update_in(cx, |this, _, cx| this.persistence_barrier(cx))
        .ok()
        .flatten();
    #[cfg(test)]
    test_gate(this, cx, TestBoundary::PublicationSubmitted).await;
    if !durable_barrier(this, cx, barrier).await {
        finish_failed_source(this, cx, source, AttachmentStorageFailure::Persistence).await;
        return;
    }
    if !is_current(this, cx, &source) {
        source.cancelled = true;
        cancel_source_after_receipt(this, cx, source).await;
        return;
    }
    let _ = this.update_in(cx, |this, window, cx| {
        let owned = this.storage_source_owned(&source);
        this.attachment_storage.forget_source(&source);
        if owned && source.owner.runtime() == &this.runtime {
            for member in source.members {
                this.stage_prepared_attachment(source.owner.clone(), member.hold.id, window, cx);
            }
        }
        cx.notify();
    });
}

fn is_current(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    source: &CapturedSource,
) -> bool {
    this.update_in(cx, |this, _, _| {
        this.attachment_storage.current(source)
            && (source.phase == SourcePhase::Proposed || this.storage_source_owned(source))
    })
    .unwrap_or(false)
}

async fn finish_failed_source(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    mut source: CapturedSource,
    failure: AttachmentStorageFailure,
) {
    source.cancelled |= !is_current(this, cx, &source);
    if source.cancelled {
        cancel_source_after_receipt(this, cx, source).await;
        return;
    }
    retain_source(this, cx, source, failure);
}

fn retain_source(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    source: CapturedSource,
    failure: AttachmentStorageFailure,
) {
    let _ = this.update_in(cx, |this, window, cx| {
        this.attachment_storage.fail(source.ticket, failure);
        if let Some(row) = this.attachment_storage.rows.get(&source.ticket) {
            this.storage_notice(&source.owner, &row.filename, window, cx);
        }
        // No citation for a source that failed before it published; one that
        // published is settled and keeps its own.
        this.retract_citation(source.ticket, window, cx);
        this.attachment_storage
            .sources
            .insert(source.key.clone(), source);
        cx.notify();
    });
}

async fn cancel_source_after_receipt(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    source: CapturedSource,
) {
    let _ = this.update_in(cx, |this, window, cx| {
        this.retract_citation(source.ticket, window, cx)
    });
    if source.phase == SourcePhase::Published {
        let _ = this.update_in(cx, |this, window, cx| {
            this.save_composer_draft(cx);
            if let Some(mut draft) = this.drafts.take(&source.owner) {
                draft.content.files.retain(|file| {
                    !source
                        .members
                        .iter()
                        .any(|member| *file == member.metadata(&source.group))
                });
                this.save_draft_preview(&source.owner, &draft, cx);
                this.drafts.save(source.owner.clone(), draft);
            }
            // This owned cancellation witness bypasses discard's work-admission guard. The
            // parent preserves all other live roots and never deletes a server temporary file.
            for member in &source.members {
                this.retire_attachment(member.hold.id.clone(), window, cx);
            }
            if source.members.iter().any(|member| {
                this.attachments
                    .get(&member.hold.id)
                    .is_some_and(|prepared| Some(prepared.blob) == member.descriptor)
                    && !this.storage_id_referenced(&member.hold.id)
            }) {
                // Capacity and close guards can refuse the parent's retirement admission.
                // Keep the canceled source actionable until every unowned descriptor retires.
                this.attachment_storage
                    .fail(source.ticket, AttachmentStorageFailure::Capacity);
                this.attachment_storage
                    .sources
                    .insert(source.key.clone(), source);
            } else {
                this.attachment_storage.forget_source(&source);
            }
            cx.notify();
        });
        return;
    }
    for index in 0..source.members.len() {
        let hold = source.members[index].hold.clone();
        let id = hold.id.clone();
        let prepared = this.update_in(cx, |this, _, cx| {
            if this.attachment_storage.holds.contains_key(&id) {
                this.attachment_storage
                    .retirements
                    .entry(id.clone())
                    .or_insert_with(|| Retirement {
                        hold,
                        ticket: source.ticket,
                        phase: RetirementPhase::Intent,
                        standalone: false,
                    });
            }
            if let Some(row) = this.attachment_storage.rows.get_mut(&source.ticket) {
                row.status = AttachmentStorageStatus::Retiring;
            }
            cx.notify();
        });
        if prepared.is_err() {
            return;
        }
        if !retire_hold(this, cx, id).await {
            retain_source(this, cx, source, AttachmentStorageFailure::Persistence);
            return;
        }
    }
    let _ = this.update_in(cx, |this, _, cx| {
        this.attachment_storage.forget_source(&source);
        cx.notify();
    });
}

async fn retire_hold(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: AttachmentId,
) -> bool {
    let plan = this
        .update_in(cx, |this, _, cx| {
            let retirement = this.attachment_storage.retirements.get(&id)?;
            let hold = retirement.hold.clone();
            let phase = retirement.phase;
            let ticket = retirement.ticket;
            if retirement.standalone {
                this.attachment_storage.active = Some(ActiveSource {
                    ticket,
                    owner: hold.owner.clone(),
                    key: Some(id.clone()),
                    group: None,
                    cancelled: true,
                });
                if let Some(row) = this.attachment_storage.rows.get_mut(&ticket) {
                    row.status = AttachmentStorageStatus::Retiring;
                }
            }
            if this.storage_blob_referenced(&id, &hold.blob) {
                this.attachment_storage
                    .fail(ticket, AttachmentStorageFailure::Referenced);
                return None;
            }
            if matches!(phase, RetirementPhase::Intent) {
                if this
                    .attachment_preparations
                    .iter()
                    .find(|existing| existing.id == id)
                    != Some(&hold)
                    || this.retired_attachment_blobs.len() >= MAX_ENTRIES
                {
                    this.attachment_storage
                        .fail(ticket, AttachmentStorageFailure::Capacity);
                    return None;
                }
                this.attachment_preparations
                    .retain(|existing| existing != &hold);
                this.retired_attachment_blobs.push(hold.blob);
                this.attachment_storage.retirements.get_mut(&id)?.phase = RetirementPhase::Release;
            }
            Some((hold, phase, this.persistence_barrier(cx)))
        })
        .ok()
        .flatten();
    let Some((hold, phase, barrier)) = plan else {
        return this
            .update_in(cx, |this, _, _| {
                !this.attachment_storage.holds.contains_key(&id)
            })
            .unwrap_or(false);
    };
    if !durable_barrier(this, cx, barrier).await {
        return false;
    }
    if !matches!(phase, RetirementPhase::Clear) {
        let receipt = this
            .update_in(cx, |this, _, _| {
                if this.storage_blob_referenced(&id, &hold.blob) {
                    return None;
                }
                this.attachment_worker()
                    .map(|worker| worker.release_attachment(hold.blob))
            })
            .ok()
            .flatten();
        let Some(receipt) = receipt else { return false };
        if !matches!(receipt.await, Ok(Ok(SpoolCommit::Durable))) {
            return false;
        }
    }
    let barrier = this
        .update_in(cx, |this, _, cx| {
            this.retired_attachment_blobs
                .retain(|blob| blob != &hold.blob);
            this.attachment_storage.retirements.get_mut(&id)?.phase = RetirementPhase::Clear;
            this.persistence_barrier(cx)
        })
        .ok()
        .flatten();
    if !durable_barrier(this, cx, barrier).await {
        return false;
    }
    this.update_in(cx, |this, _, cx| {
        if let Some(retirement) = this.attachment_storage.retirements.remove(&id)
            && retirement.standalone
        {
            this.attachment_storage.rows.remove(&retirement.ticket);
        }
        this.attachment_storage.holds.remove(&id);
        cx.notify();
        true
    })
    .unwrap_or(false)
}

#[cfg(test)]
#[derive(Default)]
struct TestGates {
    prepared: Option<TestGate>,
    put_accepted: Option<TestGate>,
    published: Option<TestGate>,
    publication_submitted: Option<TestGate>,
}
#[cfg(test)]
struct TestGate {
    reached: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}
#[cfg(test)]
enum TestBoundary {
    Prepared,
    PutAccepted,
    Published,
    PublicationSubmitted,
}
#[cfg(test)]
async fn test_gate(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    boundary: TestBoundary,
) {
    let gate = this
        .update_in(cx, |this, _, _| match boundary {
            TestBoundary::Prepared => this.attachment_storage.gates.prepared.take(),
            TestBoundary::PutAccepted => this.attachment_storage.gates.put_accepted.take(),
            TestBoundary::Published => this.attachment_storage.gates.published.take(),
            TestBoundary::PublicationSubmitted => {
                this.attachment_storage.gates.publication_submitted.take()
            }
        })
        .ok()
        .flatten();
    if let Some(gate) = gate {
        let _ = gate.reached.send(());
        let _ = gate.release.await;
    }
}

#[cfg(test)]
mod retirement_owner_tests {
    use super::*;

    use openchamber_native_core::draft::DraftTarget;
    use openchamber_native_core::ids::{RuntimeId, SessionId};

    #[test]
    fn retirement_ownership_is_exact_and_remains_until_bookkeeping_finishes() {
        let blob = BlobId::parse("00000000000000000000000000000301").expect("held local label");
        let other =
            BlobId::parse("00000000000000000000000000000302").expect("independent local label");
        let hold = PreparationHold {
            owner: DraftKey::new(
                RuntimeId::from("owner"),
                "/project",
                DraftTarget::Session(SessionId::from("ses_owner")),
            )
            .expect("original owner"),
            id: AttachmentId::from(blob.as_str()),
            generation: 1,
            blob,
        };
        let mut storage = AttachmentStorage::default();
        for phase in [
            RetirementPhase::Intent,
            RetirementPhase::Release,
            RetirementPhase::Clear,
        ] {
            storage.retirements.insert(
                hold.id.clone(),
                Retirement {
                    hold: hold.clone(),
                    phase,
                    ticket: 1,
                    standalone: true,
                },
            );
            assert!(storage.owns_retirement(&blob));
            assert!(
                !storage.owns_retirement(&other),
                "one owner never blocks an independent blob"
            );
            assert_eq!(
                storage.retirements.len(),
                1,
                "the query cannot advance retirement state"
            );
        }
        storage.retirements.remove(&hold.id);
        assert!(!storage.owns_retirement(&blob));
    }
}

#[cfg(test)]
#[path = "attachment_storage_tests.rs"]
mod tests;
