//! Attachment authority and file I/O under the mutation journal's profile lock.
//!
//! Only a validated load or a durable full save grants authority. An uncertain or
//! failed save revokes it until an explicit durable save. Pending snapshots never
//! authorize an operation. Startup retains incomplete preparation and staging
//! records without inspecting their files or contacting their server.
//! Download repair additionally requires the exact descriptor from the committed
//! transferring take, capturing prefix or unsealed composer preparation; an
//! ordinary preparation alone never grants repair authority.
//! Captured edit work additionally requires its exact committed Capturing
//! consumer. Retained Take roots preserve ordinary document authority separately.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;

use openchamber_native_transport::attachment::MAX_PREPARED_ATTACHMENT_BYTES;
use openchamber_native_transport::queue_manifest::{
    QueueIndexedPayload, QueuePayloadFileSource, QueuePayloadFilesProof,
};
use sha2::{Digest, Sha256};

use crate::attachment_catalog::{AttachmentCatalog, PreparationHold};
use crate::attachment_spool::{
    AttachmentSpool, BlobBytes, BlobDescriptor, BlobId, BlobReceipt, SpoolCommit, SpoolError,
};
use crate::mutation_journal::{
    JournalCommit, JournalError, JournalSnapshot, LoadedJournal, MutationJournal,
};
use crate::queue_capture::{
    QUEUE_CHUNK_BYTES, QueueChunkHold, QueueJson, QueueOperationId, QueueSpoolRef,
    QueueVerification,
};
use crate::queue_storage::{
    QueueAcknowledgementAuthority, QueueAction, QueueByteAuthority, QueueConsumerEditBinding,
    QueueDerivedFileState, QueueEnqueuePhase, QueueFileBinding, QueuePrefixPhase, QueueSendPhase,
    QueueTakePhase,
};

/// Only the serialized file owner constructs this after checking the whole stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueueVerifiedCapture {
    verification: QueueVerification,
}

impl QueueVerifiedCapture {
    pub(crate) fn verification(&self) -> &QueueVerification {
        &self.verification
    }
}

/// A chunk result bound to the exact consumer authorized by the file owner.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct QueueConsumerEditStoredChunk {
    binding: QueueConsumerEditBinding,
    hold: QueueChunkHold,
    receipt: BlobReceipt,
}

impl QueueConsumerEditStoredChunk {
    pub(crate) fn binding(&self) -> &QueueConsumerEditBinding {
        &self.binding
    }

    pub(crate) fn hold(&self) -> &QueueChunkHold {
        &self.hold
    }

    pub(crate) fn receipt(&self) -> &BlobReceipt {
        &self.receipt
    }
}

impl fmt::Debug for QueueConsumerEditStoredChunk {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueueConsumerEditStoredChunk")
            .field("hold", &self.hold)
            .field("receipt", &self.receipt)
            .finish_non_exhaustive()
    }
}

/// Whole-stream proof retaining the exact consumer authorized by the file owner.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct QueueConsumerEditVerifiedCapture {
    binding: QueueConsumerEditBinding,
    verification: QueueVerification,
}

impl QueueConsumerEditVerifiedCapture {
    pub(crate) fn binding(&self) -> &QueueConsumerEditBinding {
        &self.binding
    }

    pub(crate) fn verification(&self) -> &QueueVerification {
        &self.verification
    }
}

impl fmt::Debug for QueueConsumerEditVerifiedCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueueConsumerEditVerifiedCapture")
            .field("verification", &self.verification)
            .finish_non_exhaustive()
    }
}

/// A committed destination and its files were verified under the profile lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueueAcknowledgementPermit {
    operation: QueueOperationId,
    request: QueueJson,
    receipt: QueueJson,
}

impl QueueAcknowledgementPermit {
    pub(crate) fn operation(&self) -> &QueueOperationId {
        &self.operation
    }
    pub(crate) fn request(&self) -> &QueueJson {
        &self.request
    }
    pub(crate) fn receipt(&self) -> &QueueJson {
        &self.receipt
    }
}

/// Content-free errors from the committed attachment ownership boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachmentFailure {
    NoDurableAuthority,
    Unauthorized,
    Spool(SpoolError),
}

impl From<SpoolError> for AttachmentFailure {
    fn from(error: SpoolError) -> Self {
        Self::Spool(error)
    }
}

#[derive(Default)]
pub(crate) struct AttachmentAuthority {
    attachments: AttachmentCatalog,
    preparations: Vec<PreparationHold>,
    /// Revoked write permissions remain independent of retained byte roots.
    revoked_preparations: Vec<PreparationHold>,
    deleted_operations: HashSet<QueueOperationId>,
    retired_blobs: Vec<BlobId>,
    queue: QueueByteAuthority,
    /// Exact unpublished bindings from committed takes, prefixes and preparations.
    queue_attachment_holds: Vec<(PreparationHold, BlobDescriptor)>,
}

impl AttachmentAuthority {
    pub(crate) fn capture(snapshot: &JournalSnapshot) -> Self {
        let preparations: HashMap<_, _> = snapshot
            .preparations
            .iter()
            .map(|hold| (hold.blob, hold))
            .collect();
        let mut queue_attachment_holds = Vec::new();
        let mut revoked_preparations = Vec::new();
        let mut deleted_operations = HashSet::new();
        for operation in snapshot.queue.operations() {
            if snapshot.queue.local_send_deleted(operation.id()) {
                deleted_operations.insert(operation.id().clone());
                if let Some(preparation) = operation.composer_preparation()
                    && let Ok(owner) = operation.owner().draft_key()
                {
                    for file in &preparation.files {
                        if file.state == QueueDerivedFileState::Proposed
                            && let Ok(hold) = file.hold(&owner)
                            && preparations
                                .get(&hold.blob)
                                .is_some_and(|saved| **saved == hold)
                        {
                            revoked_preparations.push(hold);
                        }
                    }
                }
            }
            if matches!(
                operation.action(),
                QueueAction::Send {
                    phase: QueueSendPhase::Capturing,
                    ..
                } | QueueAction::Enqueue {
                    phase: QueueEnqueuePhase::Capturing,
                    ..
                } | QueueAction::Prefix {
                    phase: QueuePrefixPhase::Capturing,
                    ..
                }
            ) && let Some(preparation) = operation
                .composer_preparation()
                .filter(|preparation| preparation.inputs_object().is_none())
                && let Ok(owner) = operation.owner().draft_key()
                && preparation
                    .owner()
                    .as_ref()
                    .is_ok_and(|captured| captured == &owner)
                && operation.source_sha256() == Some(&preparation.source_sha256)
            {
                for file in &preparation.files {
                    if file.state != QueueDerivedFileState::Proposed
                        || file.preparation_generation != preparation.generation
                    {
                        continue;
                    }
                    let (Ok(hold), Ok(descriptor)) = (file.hold(&owner), file.descriptor()) else {
                        continue;
                    };
                    if preparations
                        .get(&hold.blob)
                        .is_some_and(|saved| **saved == hold)
                    {
                        queue_attachment_holds.push((hold, descriptor));
                    }
                }
            }
            let (items, prefix) = match operation.action() {
                QueueAction::Take {
                    phase: QueueTakePhase::Transferring(transfer),
                    ..
                } => (transfer.items.as_slice(), false),
                QueueAction::Prefix {
                    phase: QueuePrefixPhase::Capturing,
                    ..
                } => (
                    snapshot
                        .queue
                        .prefix_downloads(operation.id())
                        .unwrap_or_default(),
                    true,
                ),
                _ => continue,
            };
            let Ok(owner) = operation.owner().draft_key() else {
                continue;
            };
            for binding in items.iter().flat_map(|item| &item.files) {
                let Ok(descriptor) = binding.descriptor() else {
                    continue;
                };
                if let Some(hold) = preparations.get(descriptor.id())
                    && hold.owner == owner
                    && hold.id.as_str() == descriptor.id().as_str()
                    && (!prefix || hold.generation == 1)
                {
                    queue_attachment_holds.push(((**hold).clone(), descriptor));
                }
            }
        }
        Self {
            attachments: snapshot.attachments.clone(),
            preparations: snapshot.preparations.clone(),
            revoked_preparations,
            deleted_operations,
            retired_blobs: snapshot.retired_blobs.clone(),
            queue: snapshot.queue.byte_authority(),
            queue_attachment_holds,
        }
    }

    pub(crate) fn check_put(&self, hold: &PreparationHold) -> Result<(), AttachmentFailure> {
        if !self.preparations.contains(hold)
            || self.revoked_preparations.contains(hold)
            || self.retired_blobs.contains(&hold.blob)
        {
            return Err(AttachmentFailure::Unauthorized);
        }
        Ok(())
    }

    pub(crate) fn check_attachment_resume(
        &self,
        hold: &PreparationHold,
        expected: &BlobDescriptor,
    ) -> Result<(), AttachmentFailure> {
        self.check_put(hold)?;
        let mut bound = false;
        for (saved, descriptor) in &self.queue_attachment_holds {
            if saved == hold {
                if descriptor != expected {
                    return Err(AttachmentFailure::Unauthorized);
                }
                bound = true;
            }
        }
        if !bound
            || expected.id() != &hold.blob
            || self
                .attachments
                .values()
                .any(|attachment| attachment.blob.id() == expected.id())
            || self.queue_contains_blob(expected.id())
        {
            return Err(AttachmentFailure::Unauthorized);
        }
        Ok(())
    }

    pub(crate) fn check_read(&self, descriptor: &BlobDescriptor) -> Result<(), AttachmentFailure> {
        if !self
            .attachments
            .values()
            .any(|attachment| attachment.blob == *descriptor)
        {
            return Err(AttachmentFailure::Unauthorized);
        }
        Ok(())
    }

    pub(crate) fn check_release(&self, id: &BlobId) -> Result<(), AttachmentFailure> {
        if !self.retired_blobs.contains(id)
            || self
                .attachments
                .values()
                .any(|attachment| attachment.blob.id() == id)
            || self.preparations.iter().any(|hold| &hold.blob == id)
            || self.queue.native_roots.contains(id.as_str())
            || self.queue_contains_blob(id)
        {
            return Err(AttachmentFailure::Unauthorized);
        }
        Ok(())
    }

    fn queue_contains_blob(&self, id: &BlobId) -> bool {
        self.queue.objects.iter().any(|object| {
            object.reference().chunks().iter().any(|chunk| {
                chunk
                    .descriptor()
                    .is_ok_and(|descriptor| descriptor.id() == id)
            })
        }) || self.queue.retired.contains(id)
    }

    pub(crate) fn check_queue_put(&self, hold: &QueueChunkHold) -> Result<(), AttachmentFailure> {
        if self.deleted_operations.contains(&hold.operation) || !self.queue.holds.contains(hold) {
            return Err(AttachmentFailure::Unauthorized);
        }
        Ok(())
    }

    /// A repair cannot use a stored receipt, another object's root or a native owner.
    pub(crate) fn check_queue_resume(
        &self,
        hold: &QueueChunkHold,
    ) -> Result<(), AttachmentFailure> {
        self.check_queue_put(hold)?;
        let descriptor = hold
            .chunk
            .descriptor()
            .map_err(|_| AttachmentFailure::Unauthorized)?;
        let id = descriptor.id();
        if self.queue.stored.iter().any(|stored| stored.id() == id)
            || self.queue.objects.iter().any(|object| {
                object.id() != &hold.object
                    && object
                        .reference()
                        .chunks()
                        .iter()
                        .any(|chunk| chunk.descriptor().is_ok_and(|other| other.id() == id))
            })
            || self
                .attachments
                .values()
                .any(|attachment| attachment.blob.id() == id)
            || self
                .preparations
                .iter()
                .any(|preparation| &preparation.blob == id)
            || self.retired_blobs.contains(id)
            || self.queue.retired.contains(id)
            || self.queue.native_roots.contains(id.as_str())
        {
            return Err(AttachmentFailure::Unauthorized);
        }
        Ok(())
    }

    pub(crate) fn check_queue_read(
        &self,
        descriptor: &BlobDescriptor,
    ) -> Result<(), AttachmentFailure> {
        if !self.queue.stored.contains(descriptor) {
            return Err(AttachmentFailure::Unauthorized);
        }
        Ok(())
    }

    pub(crate) fn check_queue_verification(
        &self,
        request: &QueueVerification,
    ) -> Result<(), AttachmentFailure> {
        if self.deleted_operations.contains(&request.operation)
            || !self
                .queue
                .objects
                .iter()
                .any(|object| object.ready_to_verify() && object.verification() == *request)
        {
            return Err(AttachmentFailure::Unauthorized);
        }
        Ok(())
    }

    pub(crate) fn check_consumer_edit_put(
        &self,
        binding: &QueueConsumerEditBinding,
        hold: &QueueChunkHold,
    ) -> Result<(), AttachmentFailure> {
        self.check_queue_put(hold)?;
        if !self.queue.permits_consumer_edit(binding) || !binding.matches_chunk(hold) {
            return Err(AttachmentFailure::Unauthorized);
        }
        Ok(())
    }

    pub(crate) fn check_consumer_edit_resume(
        &self,
        binding: &QueueConsumerEditBinding,
        hold: &QueueChunkHold,
    ) -> Result<(), AttachmentFailure> {
        self.check_consumer_edit_put(binding, hold)?;
        self.check_queue_resume(hold)
    }

    pub(crate) fn check_consumer_edit_verification(
        &self,
        binding: &QueueConsumerEditBinding,
        verification: &QueueVerification,
    ) -> Result<(), AttachmentFailure> {
        self.check_queue_verification(verification)?;
        if !self.queue.permits_consumer_edit(binding) || !binding.matches_verification(verification)
        {
            return Err(AttachmentFailure::Unauthorized);
        }
        Ok(())
    }

    pub(crate) fn queue_acknowledgement(
        &self,
        operation: &QueueOperationId,
    ) -> Result<&QueueAcknowledgementAuthority, AttachmentFailure> {
        self.queue
            .acknowledgements
            .iter()
            .find(|authority| &authority.operation == operation)
            .ok_or(AttachmentFailure::Unauthorized)
    }

    pub(crate) fn check_queue_release(&self, id: &BlobId) -> Result<(), AttachmentFailure> {
        if !self.queue.retired.contains(id)
            || self.queue.objects.iter().any(|object| {
                object.reference().chunks().iter().any(|chunk| {
                    chunk
                        .descriptor()
                        .is_ok_and(|descriptor| descriptor.id() == id)
                })
            })
            || self
                .attachments
                .values()
                .any(|attachment| attachment.blob.id() == id)
            || self.preparations.iter().any(|hold| &hold.blob == id)
            || self.retired_blobs.contains(id)
            || self.queue.native_roots.contains(id.as_str())
        {
            return Err(AttachmentFailure::Unauthorized);
        }
        Ok(())
    }
}

/// The existing journal worker exclusively owns this value and both stores.
pub(crate) struct AttachmentJournal {
    journal: MutationJournal,
    spool: AttachmentSpool,
    authority: Option<AttachmentAuthority>,
}

impl AttachmentJournal {
    pub(crate) fn open(path: &Path) -> Result<(Self, LoadedJournal), JournalError> {
        // Opening the spool never precedes acquisition and validation of the journal.
        let (journal, loaded) = MutationJournal::open(path)?;
        let profile = path
            .parent()
            .ok_or(JournalError::Invalid("missing parent"))?;
        let spool = AttachmentSpool::open(profile).map_err(|error| match error {
            SpoolError::Io(kind) => JournalError::Io(kind.into()),
            SpoolError::UnsupportedPlatform => {
                JournalError::Invalid("private attachment storage is unsupported")
            }
            _ => JournalError::Invalid("private attachment storage is unsafe"),
        })?;
        let authority = Some(match &loaded {
            LoadedJournal::Missing => AttachmentAuthority::default(),
            LoadedJournal::Present(snapshot) => AttachmentAuthority::capture(snapshot),
        });
        Ok((
            Self {
                journal,
                spool,
                authority,
            },
            loaded,
        ))
    }

    pub(crate) fn save(
        &mut self,
        snapshot: &JournalSnapshot,
    ) -> Result<JournalCommit, JournalError> {
        let result = self.journal.save(snapshot);
        self.observe_save(snapshot, &result);
        result
    }

    fn observe_save(
        &mut self,
        snapshot: &JournalSnapshot,
        result: &Result<JournalCommit, JournalError>,
    ) {
        self.authority = if matches!(result, Ok(JournalCommit::Durable)) {
            Some(AttachmentAuthority::capture(snapshot))
        } else {
            // Even a pre-rename failure clears pending worker work. Only an
            // explicit successful retry may grant fresh attachment authority.
            None
        };
    }

    /// Requires the exact durable pre-put intent, including its owner and epoch.
    /// The caller journals the returned descriptor only after SpoolCommit::Durable.
    pub(crate) fn put(
        &mut self,
        hold: &PreparationHold,
        bytes: &[u8],
    ) -> Result<BlobReceipt, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_put(hold)?;
        // A proposed label has no descriptor yet. The durable receipt supplies it.
        self.spool
            .put(hold.blob, bytes, MAX_PREPARED_ATTACHMENT_BYTES)
            .map_err(Into::into)
    }

    /// The first download put requires its committed byte binding and never
    /// repairs an existing partial file. Ordinary preparation keeps its own path.
    pub(crate) fn put_downloaded_attachment(
        &mut self,
        hold: &PreparationHold,
        expected: &BlobDescriptor,
        bytes: &[u8],
    ) -> Result<BlobReceipt, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_attachment_resume(hold, expected)?;
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        if bytes.len() as u64 != expected.byte_len() || &digest != expected.sha256() {
            return Err(AttachmentFailure::Spool(SpoolError::ContentMismatch));
        }
        self.spool
            .put(hold.blob, bytes, MAX_PREPARED_ATTACHMENT_BYTES)
            .map_err(Into::into)
    }

    /// Explicit repair requires both the exact committed preparation and its
    /// original transferred descriptor. Catalog publication revokes this authority.
    pub(crate) fn resume_attachment(
        &mut self,
        hold: &PreparationHold,
        expected: &BlobDescriptor,
        bytes: &[u8],
    ) -> Result<BlobReceipt, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_attachment_resume(hold, expected)?;
        self.spool
            .resume_unpublished(expected, bytes, MAX_PREPARED_ATTACHMENT_BYTES)
            .map_err(Into::into)
    }

    /// Reads only an exact committed catalog descriptor, including its SHA-256.
    pub(crate) fn read(&self, descriptor: &BlobDescriptor) -> Result<BlobBytes, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_read(descriptor)?;
        self.spool
            .read(descriptor, MAX_PREPARED_ATTACHMENT_BYTES)
            .map_err(Into::into)
    }

    /// Explicit retirement is necessary even for partial or never-cataloged blobs.
    /// Catalog entries protect staging, sends, creations and queue-held resources.
    pub(crate) fn release(&mut self, id: &BlobId) -> Result<SpoolCommit, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_release(id)?;
        self.spool
            .release_after_durable_unreference(id)
            .map_err(Into::into)
    }

    pub(crate) fn put_queue(
        &mut self,
        hold: &QueueChunkHold,
        bytes: &[u8],
    ) -> Result<BlobReceipt, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_queue_put(hold)?;
        let expected = hold
            .chunk
            .descriptor()
            .map_err(|_| AttachmentFailure::Unauthorized)?;
        let actual: [u8; 32] = Sha256::digest(bytes).into();
        if bytes.len() as u64 != expected.byte_len() || &actual != expected.sha256() {
            return Err(AttachmentFailure::Spool(SpoolError::ContentMismatch));
        }
        self.spool
            .put(*expected.id(), bytes, QUEUE_CHUNK_BYTES)
            .map_err(Into::into)
    }

    /// Explicit retry under the exact latest committed unpublished hold. No deletion runs.
    pub(crate) fn resume_queue(
        &mut self,
        hold: &QueueChunkHold,
        bytes: &[u8],
    ) -> Result<BlobReceipt, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_queue_resume(hold)?;
        let expected = hold
            .chunk
            .descriptor()
            .map_err(|_| AttachmentFailure::Unauthorized)?;
        self.spool
            .resume_unpublished(&expected, bytes, QUEUE_CHUNK_BYTES)
            .map_err(Into::into)
    }

    pub(crate) fn read_queue(
        &self,
        descriptor: &BlobDescriptor,
    ) -> Result<BlobBytes, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_queue_read(descriptor)?;
        self.spool
            .read(descriptor, QUEUE_CHUNK_BYTES)
            .map_err(Into::into)
    }

    pub(crate) fn verify_queue(
        &self,
        verification: &QueueVerification,
    ) -> Result<QueueVerifiedCapture, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_queue_verification(verification)?;
        self.verify_queue_reference(&verification.reference)?;
        Ok(QueueVerifiedCapture {
            verification: verification.clone(),
        })
    }

    /// Consumer authorization must survive every prioritized durable save before I/O.
    pub(crate) fn put_consumer_edit_chunk(
        &mut self,
        binding: QueueConsumerEditBinding,
        hold: QueueChunkHold,
        bytes: &[u8],
    ) -> Result<QueueConsumerEditStoredChunk, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_consumer_edit_put(&binding, &hold)?;
        let receipt = self.put_queue(&hold, bytes)?;
        Ok(QueueConsumerEditStoredChunk {
            binding,
            hold,
            receipt,
        })
    }

    /// Explicit repair requires both the consumer and the existing unpublished hold.
    pub(crate) fn resume_consumer_edit_chunk(
        &mut self,
        binding: QueueConsumerEditBinding,
        hold: QueueChunkHold,
        bytes: &[u8],
    ) -> Result<QueueConsumerEditStoredChunk, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_consumer_edit_resume(&binding, &hold)?;
        let receipt = self.resume_queue(&hold, bytes)?;
        Ok(QueueConsumerEditStoredChunk {
            binding,
            hold,
            receipt,
        })
    }

    /// Produces no proof after the exact captured consumer loses its authority.
    pub(crate) fn verify_consumer_edit_capture(
        &self,
        binding: QueueConsumerEditBinding,
        verification: QueueVerification,
    ) -> Result<QueueConsumerEditVerifiedCapture, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_consumer_edit_verification(&binding, &verification)?;
        self.verify_queue_reference(&verification.reference)?;
        Ok(QueueConsumerEditVerifiedCapture {
            binding,
            verification,
        })
    }

    fn verify_queue_reference(&self, reference: &QueueSpoolRef) -> Result<(), AttachmentFailure> {
        let mut digest = Sha256::new();
        let mut total = 0u64;
        for chunk in reference.chunks() {
            let descriptor = chunk
                .descriptor()
                .map_err(|_| AttachmentFailure::Unauthorized)?;
            let bytes = self.spool.read(&descriptor, QUEUE_CHUNK_BYTES)?;
            digest.update(bytes.as_slice());
            total += bytes.as_slice().len() as u64;
        }
        let actual: [u8; 32] = digest.finalize().into();
        if total != reference.total_bytes() || &actual != reference.sha256() {
            return Err(AttachmentFailure::Spool(SpoolError::ContentMismatch));
        }
        Ok(())
    }

    pub(crate) fn authorize_queue_acknowledgement(
        &self,
        operation: &QueueOperationId,
        proofs: &[QueuePayloadFilesProof],
    ) -> Result<QueueAcknowledgementPermit, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        let acknowledgement = authority.queue_acknowledgement(operation)?;
        if proofs.len() != acknowledgement.items.len() {
            return Err(AttachmentFailure::Unauthorized);
        }
        for id in &acknowledgement.objects {
            let object = authority
                .queue
                .objects
                .iter()
                .find(|object| {
                    object.id() == id && object.operation() == operation && object.verified()
                })
                .ok_or(AttachmentFailure::Unauthorized)?;
            self.verify_queue_reference(object.reference())?;
        }
        for (item, proof) in acknowledgement.items.iter().zip(proofs) {
            // Each nonforgeable proof came from transport materializing these
            // exact capture/index seals. Metadata roots alone grant no byte proof.
            let index = QueueIndexedPayload::from_body(
                item.index.bytes(),
                &acknowledgement.directory,
                crate::queue_capture::sha256,
            )
            .map_err(|_| AttachmentFailure::Unauthorized)?;
            if proof.capture_sha256() != &index.pointer().sha256()
                || proof.index_sha256() != &index.index_hash()
                || proof.files().len() != item.files.len()
            {
                return Err(AttachmentFailure::Unauthorized);
            }
            for file in &item.files {
                let original = proof
                    .files()
                    .iter()
                    .find(|original| match (file, original.source()) {
                        (
                            QueueFileBinding::Manifest { blob, .. },
                            QueuePayloadFileSource::Manifest(expected),
                        ) => blob == expected,
                        (
                            QueueFileBinding::Legacy { ordinal, .. },
                            QueuePayloadFileSource::Legacy(expected),
                        ) => ordinal == expected,
                        _ => false,
                    })
                    .ok_or(AttachmentFailure::Unauthorized)?;
                let descriptor = file
                    .descriptor()
                    .map_err(|_| AttachmentFailure::Unauthorized)?;
                if descriptor.byte_len() != original.byte_len()
                    || descriptor.sha256() != original.sha256()
                {
                    return Err(AttachmentFailure::Spool(SpoolError::ContentMismatch));
                }
                let attachment = authority
                    .attachments
                    .get(descriptor.id().as_str())
                    .ok_or(AttachmentFailure::Unauthorized)?;
                if attachment.blob != descriptor {
                    return Err(AttachmentFailure::Unauthorized);
                }
                self.spool
                    .read(&descriptor, MAX_PREPARED_ATTACHMENT_BYTES)?;
            }
        }
        Ok(QueueAcknowledgementPermit {
            operation: operation.clone(),
            request: acknowledgement.request.clone(),
            receipt: acknowledgement.receipt.clone(),
        })
    }

    pub(crate) fn release_queue(&mut self, id: &BlobId) -> Result<SpoolCommit, AttachmentFailure> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(AttachmentFailure::NoDurableAuthority)?;
        authority.check_queue_release(id)?;
        self.spool
            .release_after_durable_unreference(id)
            .map_err(Into::into)
    }
}

#[cfg(all(test, unix))]
#[path = "attachment_io_tests.rs"]
mod composer_preparation_tests;

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use openchamber_native_core::draft::{DraftKey, DraftTarget};
    use openchamber_native_core::ids::{AttachmentId, DraftId, RuntimeId};

    use crate::attachment_catalog::PreparedAttachment;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            use std::os::unix::fs::DirBuilderExt;

            let path = PathBuf::from(format!(
                "/tmp/opencode/attachment-io-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed),
            ));
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .expect("private test profile");
            Self(path)
        }

        fn open(&self) -> (AttachmentJournal, LoadedJournal) {
            AttachmentJournal::open(&self.0.join("journal.json")).expect("open journal and spool")
        }

        fn blob_path(&self, id: &BlobId) -> PathBuf {
            self.0
                .join("attachment-spool")
                .join(format!("blob-{}.bin", id.as_str()))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn hold(value: u128) -> PreparationHold {
        let blob = BlobId::parse(&format!("{value:032x}")).expect("test blob identity");
        PreparationHold {
            owner: DraftKey::new(
                RuntimeId::from("runtime"),
                "/project",
                DraftTarget::NewSession(DraftId::from("draft")),
            )
            .expect("valid test owner"),
            id: AttachmentId::from(blob.as_str()),
            generation: 1,
            blob,
        }
    }

    fn save(journal: &mut AttachmentJournal, snapshot: &JournalSnapshot) {
        assert!(matches!(journal.save(snapshot), Ok(JournalCommit::Durable)));
    }

    #[test]
    fn durable_hold_precedes_put_and_exact_catalog_descriptor_precedes_verified_read() {
        let scratch = Scratch::new();
        let (mut journal, _) = scratch.open();
        let hold = hold(1);
        let mut snapshot = JournalSnapshot::default();
        assert_eq!(
            journal.put(&hold, b"saved"),
            Err(AttachmentFailure::Unauthorized)
        );
        snapshot.preparations.push(hold.clone());
        // An in-memory intent alone creates no authority or blob file.
        assert_eq!(
            journal.put(&hold, b"saved"),
            Err(AttachmentFailure::Unauthorized)
        );
        assert!(!scratch.blob_path(&hold.blob).exists());
        save(&mut journal, &snapshot);
        let receipt = journal
            .put(&hold, b"saved")
            .expect("durable hold authorizes captured bytes");
        assert_eq!(receipt.commit, SpoolCommit::Durable);
        assert!(matches!(
            journal.read(&receipt.descriptor),
            Err(AttachmentFailure::Unauthorized)
        ));
        snapshot.attachments.insert(
            hold.id.clone(),
            PreparedAttachment {
                blob: receipt.descriptor,
                staging: None,
            },
        );
        save(&mut journal, &snapshot);
        assert_eq!(
            journal
                .read(&receipt.descriptor)
                .expect("catalog permits read")
                .as_slice(),
            b"saved"
        );
        let wrong = BlobDescriptor::new(hold.blob, 5, [0; 32]);
        assert!(matches!(
            journal.read(&wrong),
            Err(AttachmentFailure::Unauthorized)
        ));
        fs::write(scratch.blob_path(&hold.blob), b"other").expect("same-length corruption");
        assert!(matches!(
            journal.read(&receipt.descriptor),
            Err(AttachmentFailure::Spool(SpoolError::ContentMismatch))
        ));
    }

    #[test]
    fn maximum_prepared_repeat_uses_the_same_captured_buffer_and_committed_hold() {
        let scratch = Scratch::new();
        let (mut journal, _) = scratch.open();
        let hold = hold(8);
        save(
            &mut journal,
            &JournalSnapshot {
                preparations: vec![hold.clone()],
                ..JournalSnapshot::default()
            },
        );
        let bytes = vec![0x5a; MAX_PREPARED_ATTACHMENT_BYTES];
        let original = journal.put(&hold, &bytes).expect("maximum prepared put");
        assert_eq!(original.commit, SpoolCommit::Durable);
        let repeated = journal.put(&hold, &bytes).expect("maximum prepared repeat");
        assert_eq!(repeated, original);
        assert_eq!(
            fs::metadata(scratch.blob_path(&hold.blob))
                .expect("unchanged blob")
                .len(),
            MAX_PREPARED_ATTACHMENT_BYTES as u64
        );
        assert!(
            matches!(
                journal.read(&original.descriptor),
                Err(AttachmentFailure::Unauthorized)
            ),
            "a repeated put still grants no catalog read authority"
        );
    }

    #[test]
    fn retirement_requires_durable_explicit_intent_and_checks_holds_and_catalog() {
        let scratch = Scratch::new();
        let (mut journal, _) = scratch.open();
        let hold = hold(2);
        let mut snapshot = JournalSnapshot {
            preparations: vec![hold.clone()],
            ..JournalSnapshot::default()
        };
        save(&mut journal, &snapshot);
        let receipt = journal.put(&hold, b"saved").expect("put held blob");
        assert_eq!(
            journal.release(&hold.blob),
            Err(AttachmentFailure::Unauthorized)
        );
        snapshot.preparations.clear();
        snapshot.attachments.insert(
            hold.id.clone(),
            PreparedAttachment {
                blob: receipt.descriptor,
                staging: None,
            },
        );
        save(&mut journal, &snapshot);
        assert_eq!(
            journal.release(&hold.blob),
            Err(AttachmentFailure::Unauthorized)
        );
        snapshot.attachments.clear();
        save(&mut journal, &snapshot);
        assert_eq!(
            journal.release(&hold.blob),
            Err(AttachmentFailure::Unauthorized)
        );
        snapshot.retired_blobs.push(hold.blob);
        assert_eq!(
            journal.release(&hold.blob),
            Err(AttachmentFailure::Unauthorized)
        );
        save(&mut journal, &snapshot);
        assert_eq!(journal.release(&hold.blob), Ok(SpoolCommit::Durable));
        assert!(!scratch.blob_path(&hold.blob).exists());
        assert_eq!(journal.release(&hold.blob), Ok(SpoolCommit::Durable));

        // Inspect both guards directly even if malformed authority cannot be saved.
        journal
            .authority
            .as_mut()
            .expect("durable authority")
            .preparations
            .push(hold.clone());
        assert_eq!(
            journal.release(&hold.blob),
            Err(AttachmentFailure::Unauthorized)
        );
        let authority = journal.authority.as_mut().expect("durable authority");
        authority.preparations.clear();
        authority.attachments.insert(
            hold.id,
            PreparedAttachment {
                blob: receipt.descriptor,
                staging: None,
            },
        );
        assert_eq!(
            journal.release(&hold.blob),
            Err(AttachmentFailure::Unauthorized)
        );
    }

    #[test]
    fn changed_owner_generation_and_removed_hold_cancel_captured_puts() {
        let scratch = Scratch::new();
        let (mut journal, _) = scratch.open();
        let hold = hold(3);
        let mut snapshot = JournalSnapshot {
            preparations: vec![hold.clone()],
            ..JournalSnapshot::default()
        };
        save(&mut journal, &snapshot);
        let mut stale = hold.clone();
        stale.generation += 1;
        assert_eq!(
            journal.put(&stale, b"saved"),
            Err(AttachmentFailure::Unauthorized)
        );
        stale = hold.clone();
        stale.owner = DraftKey::new(
            RuntimeId::from("other"),
            "/project",
            DraftTarget::NewSession(DraftId::from("draft")),
        )
        .expect("other owner");
        assert_eq!(
            journal.put(&stale, b"saved"),
            Err(AttachmentFailure::Unauthorized)
        );
        snapshot.preparations.clear();
        save(&mut journal, &snapshot);
        assert_eq!(
            journal.put(&hold, b"saved"),
            Err(AttachmentFailure::Unauthorized)
        );
        assert!(!scratch.blob_path(&hold.blob).exists());
    }

    #[test]
    fn uncertain_or_failed_save_revokes_authority_until_explicit_durable_save() {
        let scratch = Scratch::new();
        let (mut journal, _) = scratch.open();
        let hold = hold(4);
        let snapshot = JournalSnapshot {
            preparations: vec![hold.clone()],
            ..JournalSnapshot::default()
        };
        save(&mut journal, &snapshot);
        let receipt = journal.put(&hold, b"saved").expect("put held blob");
        // Inject the post-rename directory-sync outcome at the exact save boundary.
        let committed = journal.journal.save(&snapshot);
        assert!(matches!(committed, Ok(JournalCommit::Durable)));
        journal.observe_save(
            &snapshot,
            &Ok(JournalCommit::DirectorySyncUncertain(
                std::io::Error::other("private diagnostic"),
            )),
        );
        assert_eq!(
            journal.put(&hold, b"saved"),
            Err(AttachmentFailure::NoDurableAuthority)
        );
        assert_eq!(
            journal.release(&hold.blob),
            Err(AttachmentFailure::NoDurableAuthority)
        );
        assert!(matches!(
            journal.read(&receipt.descriptor),
            Err(AttachmentFailure::NoDurableAuthority)
        ));
        save(&mut journal, &snapshot);
        assert!(journal.put(&hold, b"saved").is_ok());
        journal.observe_save(&snapshot, &Err(JournalError::Invalid("injected failure")));
        assert_eq!(
            journal.put(&hold, b"saved"),
            Err(AttachmentFailure::NoDurableAuthority)
        );
        save(&mut journal, &snapshot);
        assert!(journal.put(&hold, b"saved").is_ok());
    }

    #[test]
    fn startup_preserves_incomplete_holds_and_retirement_without_automatic_io() {
        let scratch = Scratch::new();
        let (mut journal, _) = scratch.open();
        let retired = hold(6).blob;
        let hold = hold(5);
        save(
            &mut journal,
            &JournalSnapshot {
                preparations: vec![hold.clone()],
                retired_blobs: vec![retired],
                ..JournalSnapshot::default()
            },
        );
        // Partial bytes and unknown entries must survive reopen untouched.
        fs::write(scratch.blob_path(&hold.blob), b"partial").expect("partial held file");
        fs::write(scratch.blob_path(&retired), b"retired").expect("pending release file");
        drop(journal);
        let (journal, loaded) = scratch.open();
        let LoadedJournal::Present(snapshot) = loaded else {
            panic!("saved journal exists");
        };
        assert_eq!(snapshot.preparations, vec![hold.clone()]);
        assert_eq!(
            fs::read(scratch.blob_path(&hold.blob)).expect("hold retained"),
            b"partial"
        );
        assert_eq!(
            fs::read(scratch.blob_path(&retired)).expect("retirement needs explicit release"),
            b"retired"
        );
        drop(journal);
    }

    #[test]
    fn journal_lock_and_validation_precede_any_spool_creation() {
        let scratch = Scratch::new();
        let path = scratch.0.join("journal.json");
        let (journal, _) = MutationJournal::open(&path).expect("hold profile lock without spool");
        assert!(matches!(
            AttachmentJournal::open(&path),
            Err(JournalError::Locked)
        ));
        assert!(!scratch.0.join("attachment-spool").exists());
        drop(journal);
        use std::os::unix::fs::PermissionsExt;
        fs::write(&path, b"malformed private journal").expect("malformed test journal");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("private journal mode");
        assert!(AttachmentJournal::open(&path).is_err());
        assert!(!scratch.0.join("attachment-spool").exists());
    }

    #[test]
    fn real_worker_round_trip_verifies_bytes_and_drains_release_before_unlocking() {
        use crate::journal_worker::{BlobFailure, CommitOutcome, JournalFailure, JournalWorker};

        let scratch = Scratch::new();
        let path = scratch.0.join("journal.json");
        let (worker, hydration) = JournalWorker::start(path.clone());
        assert!(matches!(
            hydration.blocking_recv(),
            Ok(Ok(LoadedJournal::Missing))
        ));
        let hold = hold(7);
        let put = |bytes: Vec<u8>| {
            worker.put_attachment(
                hold.owner.clone(),
                hold.id.clone(),
                hold.generation,
                hold.blob,
                bytes,
            )
        };
        assert_eq!(
            put(b"saved".to_vec())
                .blocking_recv()
                .expect("early put answered"),
            Err(BlobFailure::Attachment(AttachmentFailure::Unauthorized))
        );
        let mut snapshot = JournalSnapshot {
            preparations: vec![hold.clone()],
            ..JournalSnapshot::default()
        };
        assert_eq!(
            worker
                .submit(JournalSnapshot {
                    preparations: vec![hold.clone()],
                    ..JournalSnapshot::default()
                })
                .blocking_recv()
                .expect("intent receipt"),
            Ok(CommitOutcome::Durable)
        );
        let receipt = put(b"saved".to_vec())
            .blocking_recv()
            .expect("put answered")
            .expect("durable put");
        assert_eq!(receipt.commit, SpoolCommit::Durable);
        assert!(matches!(
            worker.read_attachment(receipt.descriptor).blocking_recv(),
            Ok(Err(BlobFailure::Attachment(
                AttachmentFailure::Unauthorized
            )))
        ));
        snapshot.attachments.insert(
            hold.id.clone(),
            PreparedAttachment {
                blob: receipt.descriptor,
                staging: None,
            },
        );
        assert_eq!(
            worker
                .submit(snapshot)
                .blocking_recv()
                .expect("catalog receipt"),
            Ok(CommitOutcome::Durable)
        );
        assert_eq!(
            worker
                .read_attachment(receipt.descriptor)
                .blocking_recv()
                .expect("read answered")
                .expect("verified read")
                .as_slice(),
            b"saved"
        );
        fs::write(scratch.blob_path(&hold.blob), b"other").expect("same-length corruption");
        assert!(matches!(
            worker.read_attachment(receipt.descriptor).blocking_recv(),
            Ok(Err(BlobFailure::Attachment(AttachmentFailure::Spool(
                SpoolError::ContentMismatch
            ))))
        ));
        assert_eq!(
            worker
                .flush()
                .blocking_recv()
                .expect("flush after read failure"),
            Ok(CommitOutcome::Durable)
        );
        let retired = JournalSnapshot {
            retired_blobs: vec![hold.blob],
            ..JournalSnapshot::default()
        };
        assert_eq!(
            worker
                .submit(retired)
                .blocking_recv()
                .expect("retirement receipt"),
            Ok(CommitOutcome::Durable)
        );
        let release = worker.release_attachment(hold.blob);
        let finish = worker.finish();
        assert_eq!(
            release.blocking_recv().expect("release answered"),
            Ok(SpoolCommit::Durable)
        );
        assert_eq!(
            finish.blocking_recv().expect("finish answered"),
            Ok(CommitOutcome::Durable)
        );
        assert_eq!(
            worker
                .release_attachment(hold.blob)
                .blocking_recv()
                .expect("closed receipt"),
            Err(BlobFailure::Journal(JournalFailure::Closed))
        );
        assert!(!scratch.blob_path(&hold.blob).exists());
        let (journal, _) =
            MutationJournal::open(&path).expect("finish released exclusive profile lock");
        drop(journal);
    }

    #[test]
    fn unread_maximum_read_reply_stays_charged_until_consumed_or_dropped() {
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll, Waker};

        use crate::journal_worker::{BlobFailure, CommitOutcome, JournalFailure, JournalWorker};

        let scratch = Scratch::new();
        let path = scratch.0.join("journal.json");
        let (mut journal, _) = scratch.open();
        let hold = hold(9);
        let mut snapshot = JournalSnapshot {
            preparations: vec![hold.clone()],
            ..JournalSnapshot::default()
        };
        save(&mut journal, &snapshot);
        let captured = vec![0x5a; MAX_PREPARED_ATTACHMENT_BYTES];
        let descriptor = journal
            .put(&hold, &captured)
            .expect("durable maximum blob")
            .descriptor;
        drop(captured);
        snapshot.attachments.insert(
            hold.id,
            PreparedAttachment {
                blob: descriptor,
                staging: None,
            },
        );
        save(&mut journal, &snapshot);
        drop(journal);

        let (worker, hydration) = JournalWorker::start(path.clone());
        assert!(matches!(
            hydration.blocking_recv(),
            Ok(Ok(LoadedJournal::Present(_)))
        ));
        let mut first = worker.read_attachment(descriptor);
        assert_eq!(
            worker.flush().blocking_recv().expect("read completed"),
            Ok(CommitOutcome::Durable)
        );
        assert!(matches!(
            worker.read_attachment(descriptor).blocking_recv(),
            Ok(Err(BlobFailure::Journal(JournalFailure::AtCapacity)))
        ));

        let mut context = Context::from_waker(Waker::noop());
        let Poll::Ready(Ok(Ok(caller_bytes))) = Pin::new(&mut first).poll(&mut context) else {
            panic!("flush completed the real read before polling its receipt");
        };
        assert_eq!(caller_bytes.as_slice().len(), MAX_PREPARED_ATTACHMENT_BYTES);
        assert!(caller_bytes.as_slice().iter().all(|byte| *byte == 0x5a));

        // Keeping the completed Future and caller-owned bytes does not retain
        // the worker reservation. Only a new unread reply occupies its budget.
        let second = worker.read_attachment(descriptor);
        assert_eq!(
            worker
                .flush()
                .blocking_recv()
                .expect("second read completed"),
            Ok(CommitOutcome::Durable)
        );
        assert!(matches!(
            worker.read_attachment(descriptor).blocking_recv(),
            Ok(Err(BlobFailure::Journal(JournalFailure::AtCapacity)))
        ));
        drop(second);
        let third = worker.read_attachment(descriptor);
        let finish = worker.finish();
        assert_eq!(
            finish.blocking_recv().expect("finish ignores unread reply"),
            Ok(CommitOutcome::Durable)
        );
        let (journal, _) =
            MutationJournal::open(&path).expect("unread reply holds no profile lock");
        drop(journal);
        let final_bytes = third
            .blocking_recv()
            .expect("reply survives finish")
            .expect("verified bytes");
        assert_eq!(final_bytes.as_slice(), caller_bytes.as_slice());
        drop(first);
    }
}
