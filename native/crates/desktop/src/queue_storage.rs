//! Durable queue operation ownership. This module performs no file or network I/O.
//!
//! Initial enqueues retain the complete native source. Only a verified companion
//! replaces it. Transfers retain server ownership until a complete local destination
//! is committed; only the worker can turn that committed state into an ack permit.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};

use openchamber_native_core::draft::{DraftKey, DraftTarget};
use openchamber_native_core::draft_content::DraftFileContent;
use openchamber_native_core::ids::{AttachmentId, MessageId, RuntimeId, SessionId};
use openchamber_native_core::message_queue::QueuedSendConfig;
use openchamber_native_transport::prompt::Delivery;
use openchamber_native_transport::queue_manifest::{
    QueueIndexedPayload, QueuePayloadPointer, QueuePayloadPrefix, QueuePayloadTakeRecord,
    QueuePayloadTakeState,
};
use serde::{Deserialize, Serialize};

use crate::attachment_catalog::{AttachmentCatalog, PreparationHold, PreparedAttachment};
use crate::attachment_io::{
    QueueAcknowledgementPermit, QueueConsumerEditVerifiedCapture, QueueVerifiedCapture,
};
use crate::attachment_spool::{BlobDescriptor, BlobId, BlobReceipt, SpoolCommit};
use crate::draft_companion::CompanionCapture;
use crate::draft_storage::required_option;
use crate::mutation_journal::{JournalError, MAX_DIRECTORY, MAX_ENTRIES, Owner, identifier};
use crate::queue_capture::{
    QueueChunkHold, QueueJson, QueueNativeSource, QueueObjectId, QueueObjectKind, QueueOperationId,
    QueueSpoolRef, QueueVerification, sha256,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueOwner {
    runtime: String,
    directory: String,
    session: String,
    draft: Owner,
}

impl QueueOwner {
    pub(crate) fn new(
        runtime: &RuntimeId,
        directory: &str,
        session: &SessionId,
        draft: &DraftKey,
    ) -> Result<Self, JournalError> {
        let owner = Self {
            runtime: runtime.to_string(),
            directory: directory.to_owned(),
            session: session.to_string(),
            draft: Owner::capture(draft),
        };
        owner.validate()?;
        Ok(owner)
    }

    pub(crate) fn runtime(&self) -> &str {
        &self.runtime
    }
    pub(crate) fn directory(&self) -> &str {
        &self.directory
    }
    pub(crate) fn session(&self) -> &str {
        &self.session
    }
    pub(crate) fn draft_key(&self) -> Result<DraftKey, JournalError> {
        self.draft.clone().into_key()
    }

    fn validate(&self) -> Result<(), JournalError> {
        identifier(&self.runtime)?;
        identifier(&self.session)?;
        if !self.session.starts_with("ses_")
            || self.directory.is_empty()
            || self.directory.len() > MAX_DIRECTORY
            || self.directory.trim() != self.directory
            || self.directory.chars().any(char::is_control)
        {
            return Err(JournalError::Invalid("invalid exact queue owner"));
        }
        let draft = self.draft_key()?;
        if draft.runtime().as_str() != self.runtime
            || draft.target() != &DraftTarget::Session(SessionId::from(self.session.clone()))
            || crate::attachment_catalog::remote_path(&self.directory).as_deref()
                != Some(draft.directory())
        {
            return Err(JournalError::Invalid(
                "queue owner differs from native draft",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueConfiguration {
    provider: String,
    model: String,
    #[serde(deserialize_with = "required_option")]
    agent: Option<String>,
    #[serde(deserialize_with = "required_option")]
    variant: Option<String>,
}

impl QueueConfiguration {
    pub(crate) fn capture(config: &QueuedSendConfig) -> Self {
        Self {
            provider: config.provider_id.clone(),
            model: config.model_id.clone(),
            agent: config.agent.clone(),
            variant: config.variant.clone(),
        }
    }

    pub(crate) fn restore(&self) -> QueuedSendConfig {
        QueuedSendConfig {
            provider_id: self.provider.clone(),
            model_id: self.model.clone(),
            agent: self.agent.clone(),
            variant: self.variant.clone(),
        }
    }

    fn validate(&self) -> Result<(), JournalError> {
        for field in [&self.provider, &self.model]
            .into_iter()
            .chain(self.agent.iter())
            .chain(self.variant.iter())
        {
            identifier(field)?;
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum QueueSource {
    Inline(Box<QueueNativeSource>),
    Companion(QueueObjectId),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum QueueTakePurpose {
    Edit,
    Send {
        message: String,
        #[serde(deserialize_with = "required_option")]
        delivery: Option<Delivery>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum QueueDestination {
    Draft {
        companion: QueueObjectId,
    },
    Send {
        companion: QueueObjectId,
        prompt: QueueObjectId,
        message: String,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum QueueFileBinding {
    Manifest {
        blob: [u8; 32],
        local: String,
        byte_len: u64,
        sha256: [u8; 32],
    },
    Legacy {
        ordinal: usize,
        local: String,
        byte_len: u64,
        sha256: [u8; 32],
    },
}

impl QueueFileBinding {
    fn local(&self) -> &str {
        match self {
            Self::Manifest { local, .. } | Self::Legacy { local, .. } => local,
        }
    }

    pub(crate) fn descriptor(&self) -> Result<BlobDescriptor, JournalError> {
        let (local, byte_len, sha256) = match self {
            Self::Manifest {
                local,
                byte_len,
                sha256,
                ..
            }
            | Self::Legacy {
                local,
                byte_len,
                sha256,
                ..
            } => (local, *byte_len, *sha256),
        };
        if byte_len > openchamber_native_transport::attachment::MAX_PREPARED_ATTACHMENT_BYTES as u64
        {
            return Err(JournalError::Invalid(
                "queue attachment binding exceeds its byte limit",
            ));
        }
        let id = BlobId::parse(local)
            .map_err(|_| JournalError::Invalid("queue attachment has no local blob identity"))?;
        Ok(BlobDescriptor::new(id, byte_len, sha256))
    }
}

/// The exact index remains opaque until transport materializes the active capture.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueTransferItem {
    pub(crate) item: String,
    pub(crate) index: QueueJson,
    pub(crate) payload: QueueObjectId,
    pub(crate) files: Vec<QueueFileBinding>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueTransfer {
    /// Compact native envelope of the typed receipt, without handoff payloads.
    pub(crate) receipt: QueueJson,
    pub(crate) items: Vec<QueueTransferItem>,
}

impl QueueTransfer {
    pub(crate) fn capture_receipt(
        record: &QueuePayloadTakeRecord,
    ) -> Result<QueueJson, JournalError> {
        let receipt = &record.receipt;
        let identity = &receipt.identity;
        let state = match &record.state {
            QueuePayloadTakeState::Transferring { .. } => "transferring",
            QueuePayloadTakeState::Acknowledged => "acknowledged",
        };
        let body = serde_json::json!({
            "receipt": { "version": 1, "kind": "take", "requestId": identity.request_id,
                "sessionId": identity.session_id.as_str(), "directory": identity.directory,
                "requestHash": hex_digest(&identity.request_hash), "acceptedAt": identity.accepted_at,
                "itemIds": receipt.item_ids.iter().map(|id| id.as_str()).collect::<Vec<_>>() },
            "state": state, "revision": record.revision,
        });
        QueueJson::new(
            serde_json::to_vec(&body)
                .map_err(|_| JournalError::Invalid("queue receipt cannot be encoded"))?,
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueLocalTake {
    pub(crate) transfer: QueueTransfer,
    pub(crate) destination: QueueDestination,
    pub(crate) acknowledge: QueueJson,
    #[serde(default)]
    pub(crate) send: QueueTakeSendState,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueueTakeSendState {
    #[default]
    Prepared,
    Unknown,
    Admitted,
}

/// A first-class composer owner. Its expanded content lives in the verified spool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DraftCompanionRef {
    pub(crate) operation: QueueOperationId,
    pub(crate) companion: QueueObjectId,
}

/// Missing in older v8 snapshots, whose prior source use cannot be inferred.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum QueueTakeSourceUse {
    Unused,
    Claimed { consumer: QueueOperationId },
}

/// One immutable read-only occurrence of an original acknowledged Edit item.
/// Native-first source_index starts at 1. Only working context IDs are rebound;
/// payload, index, file proof and the consuming source seal remain original.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueSourceOccurrence {
    pub(crate) source_index: usize,
    pub(crate) take: QueueOperationId,
    pub(crate) item_ordinal: usize,
    pub(crate) source_sha256: [u8; 32],
    pub(crate) payload_sha256: [u8; 32],
    pub(crate) index_sha256: [u8; 32],
    /// None is exact first use. Some([]) is an explicit context-free repeat.
    #[serde(deserialize_with = "required_option")]
    pub(crate) context_rebinding: Option<Vec<String>>,
}

/// Exact local editing snapshot. This reference grants no server queue authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComposerEditRef {
    pub(crate) operation: QueueOperationId,
    pub(crate) object: QueueObjectId,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) byte_len: u64,
    pub(crate) sha256: [u8; 32],
    pub(crate) companions: Vec<DraftCompanionRef>,
}

/// Exact consuming operation authority, independent of the edit's original Take.
/// Only the validated ledger constructs this nonserialized request binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueueConsumerEditBinding {
    consumer: QueueOperationId,
    source_sha256: [u8; 32],
    edit: Arc<ComposerEditRef>,
}

impl QueueConsumerEditBinding {
    pub(crate) fn consumer(&self) -> &QueueOperationId {
        &self.consumer
    }
    pub(crate) fn edit(&self) -> &ComposerEditRef {
        &self.edit
    }
    pub(crate) fn matches_chunk(&self, hold: &QueueChunkHold) -> bool {
        hold.operation == self.edit.operation
            && hold.object == self.edit.object
            && hold.generation == self.edit.generation
    }
    pub(crate) fn matches_verification(&self, verification: &QueueVerification) -> bool {
        verification.operation == self.edit.operation
            && verification.object == self.edit.object
            && verification.generation == self.edit.generation
            && verification.reference.total_bytes() == self.edit.byte_len
            && verification.reference.sha256() == &self.edit.sha256
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(std::mem::size_of::<ComposerEditRef>())
            .saturating_add(self.consumer.as_str().len())
            .saturating_add(self.edit.operation.as_str().len())
            .saturating_add(self.edit.object.as_str().len())
            .saturating_add(
                self.edit
                    .companions
                    .capacity()
                    .saturating_mul(std::mem::size_of::<DraftCompanionRef>()),
            )
            .saturating_add(
                self.edit
                    .companions
                    .iter()
                    .fold(0usize, |total, reference| {
                        total
                            .saturating_add(reference.operation.as_str().len())
                            .saturating_add(reference.companion.as_str().len())
                    }),
            )
    }
}

/// Window-local custody captured while these genuine companions are current.
/// Claims and occurrence mappings are deliberately absent until admission.
pub(crate) struct CapturedComposerSource {
    consumer: QueueOperationId,
    owner: DraftKey,
    companions: Vec<DraftCompanionRef>,
    edit: Option<ComposerEditRef>,
    native_roots: Vec<AttachmentId>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposerEditRecord {
    owner: Owner,
    current: Option<ComposerEditRef>,
    /// Old and partially written versions remain roots until explicit retirement.
    versions: Vec<ComposerEditRef>,
}

/// Native file authority captured before staging or queue uploads begin.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueNativeFileDescriptor {
    pub(crate) id: String,
    pub(crate) byte_len: u64,
    pub(crate) sha256: [u8; 32],
}

/// A source ordinal is native-first, followed by the original companion items.
/// Literal ordinals identify pieces in the captured editing document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum QueuePreparationSourceId {
    Source(usize),
    Literal(usize),
}

/// Exact pre-expansion input, checked against captured source/edit bytes by the
/// owner callback before admission. Model-facing file names grant no authority.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueuePreparationSource {
    pub(crate) identity: QueuePreparationSourceId,
    pub(crate) byte_len: u64,
    pub(crate) sha256: [u8; 32],
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueueDerivedFileState {
    Proposed,
    Cataloged,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueuePreparationMention {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) text: String,
}

/// A derived byte output is tied to an explicit input mention and an exact
/// resolved server path. Reference-only prompt parts never become these records.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueDerivedFile {
    pub(crate) source: QueuePreparationSourceId,
    pub(crate) mention: QueuePreparationMention,
    pub(crate) server_path: String,
    pub(crate) raw_source_byte_len: u64,
    pub(crate) raw_source_sha256: [u8; 32],
    pub(crate) output_ordinal: usize,
    pub(crate) local_attachment_id: String,
    pub(crate) blob: String,
    pub(crate) preparation_generation: u64,
    pub(crate) byte_len: u64,
    pub(crate) sha256: [u8; 32],
    pub(crate) name: String,
    pub(crate) mime_type: String,
    pub(crate) document_group: Option<String>,
    pub(crate) state: QueueDerivedFileState,
}

impl QueueDerivedFile {
    pub(crate) fn descriptor(&self) -> Result<BlobDescriptor, JournalError> {
        let blob = BlobId::parse(&self.blob)
            .map_err(|_| JournalError::Invalid("invalid derived attachment blob"))?;
        if self.local_attachment_id != self.blob
            || self.byte_len
                > openchamber_native_transport::attachment::MAX_PREPARED_ATTACHMENT_BYTES as u64
        {
            return Err(JournalError::Invalid(
                "invalid derived attachment descriptor",
            ));
        }
        Ok(BlobDescriptor::new(blob, self.byte_len, self.sha256))
    }

    pub(crate) fn hold(&self, owner: &DraftKey) -> Result<PreparationHold, JournalError> {
        Ok(PreparationHold {
            owner: owner.clone(),
            id: AttachmentId::from(self.local_attachment_id.clone()),
            generation: self.preparation_generation,
            blob: *self.descriptor()?.id(),
        })
    }
}

/// Preparation never rewrites source_intent, source_seal or the original roots.
/// Before sealing, extensions preserve every existing record and append only.
/// Publication changes proposed-to-cataloged receipts and binds inputs once.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueComposerPreparation {
    owner: Owner,
    pub(crate) source_sha256: [u8; 32],
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) sources: Vec<QueuePreparationSource>,
    pub(crate) files: Vec<QueueDerivedFile>,
    pub(crate) derived_context_ids: Vec<String>,
    inputs_object: Option<QueueObjectId>,
}

impl QueueComposerPreparation {
    pub(crate) fn new(
        owner: &DraftKey,
        source_sha256: [u8; 32],
        generation: u64,
        revision: u64,
        sources: Vec<QueuePreparationSource>,
        files: Vec<QueueDerivedFile>,
        derived_context_ids: Vec<String>,
    ) -> Self {
        Self {
            owner: Owner::capture(owner),
            source_sha256,
            generation,
            revision,
            sources,
            files,
            derived_context_ids,
            inputs_object: None,
        }
    }

    pub(crate) fn owner(&self) -> Result<DraftKey, JournalError> {
        self.owner.clone().into_key()
    }
    pub(crate) fn inputs_object(&self) -> Option<&QueueObjectId> {
        self.inputs_object.as_ref()
    }
}

/// Which editing revision remains after a published consumer takes its capture.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerConsumption {
    Consume,
    PreserveCurrent,
}

/// Durable local cleanup continuation. It grants no message, file-write or
/// acknowledgement authority and does not count as a live attachment consumer.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueLocalSendRetirement {
    pub(crate) operation: QueueOperationId,
    pub(crate) owner: QueueOwner,
    pub(crate) dependencies: Vec<QueueOperationId>,
    pub(crate) files: Vec<QueueNativeFileDescriptor>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) deleted: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum NativePrefixKind {
    #[serde(rename = "native-prefix-capture")]
    Capture,
}

/// Internal capture intent. This representation is never an HTTP request.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativePrefixIntent {
    pub(crate) kind: NativePrefixKind,
    pub(crate) directory: String,
    pub(crate) id: String,
    #[serde(rename = "itemIds")]
    pub(crate) item_ids: Vec<String>,
    #[serde(deserialize_with = "required_option")]
    pub(crate) delivery: Option<Delivery>,
    pub(crate) companions: Vec<DraftCompanionRef>,
    pub(crate) native_context_ids: Vec<String>,
    pub(crate) files: Vec<QueueNativeFileDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) edit: Option<ComposerEditRef>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum NativeSendKind {
    #[serde(rename = "native-local-send-capture")]
    Capture,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSendIntent {
    pub(crate) kind: NativeSendKind,
    pub(crate) directory: String,
    pub(crate) id: String,
    #[serde(deserialize_with = "required_option")]
    pub(crate) delivery: Option<Delivery>,
    pub(crate) companions: Vec<DraftCompanionRef>,
    pub(crate) native_context_ids: Vec<String>,
    pub(crate) files: Vec<QueueNativeFileDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) edit: Option<ComposerEditRef>,
}

/// Exact source object. Native content and ordered dependencies are sealed together.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrefixSourceCapture {
    pub(crate) version: u32,
    pub(crate) composer: CompanionCapture,
    pub(crate) intent: NativePrefixIntent,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendSourceCapture {
    pub(crate) version: u32,
    pub(crate) composer: CompanionCapture,
    pub(crate) intent: NativeSendIntent,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum NativeEnqueueKind {
    #[serde(rename = "native-local-enqueue-capture")]
    Capture,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeEnqueueIntent {
    pub(crate) kind: NativeEnqueueKind,
    pub(crate) directory: String,
    pub(crate) companions: Vec<DraftCompanionRef>,
    pub(crate) native_context_ids: Vec<String>,
    pub(crate) files: Vec<QueueNativeFileDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) edit: Option<ComposerEditRef>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnqueueSourceCapture {
    pub(crate) version: u32,
    pub(crate) composer: CompanionCapture,
    pub(crate) intent: NativeEnqueueIntent,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum QueueSourceIntent {
    Enqueue(NativeEnqueueIntent),
    Prefix(NativePrefixIntent),
    Send(NativeSendIntent),
}

impl QueueSourceIntent {
    fn edit(&self) -> Option<&ComposerEditRef> {
        match self {
            Self::Enqueue(intent) => intent.edit.as_ref(),
            Self::Prefix(intent) => intent.edit.as_ref(),
            Self::Send(intent) => intent.edit.as_ref(),
        }
    }
    fn companions(&self) -> &[DraftCompanionRef] {
        match self {
            Self::Enqueue(intent) => &intent.companions,
            Self::Prefix(intent) => &intent.companions,
            Self::Send(intent) => &intent.companions,
        }
    }

    fn native_context_ids(&self) -> &[String] {
        match self {
            Self::Enqueue(intent) => &intent.native_context_ids,
            Self::Prefix(intent) => &intent.native_context_ids,
            Self::Send(intent) => &intent.native_context_ids,
        }
    }

    fn files(&self) -> &[QueueNativeFileDescriptor] {
        match self {
            Self::Enqueue(intent) => &intent.files,
            Self::Prefix(intent) => &intent.files,
            Self::Send(intent) => &intent.files,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueSourceSeal {
    byte_len: u64,
    sha256: [u8; 32],
}

/// Local admission has no backend queue operation or acknowledgement authority.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum QueueSendPhase {
    Capturing,
    Prepared {
        frame: QueueObjectId,
    },
    Unknown {
        frame: QueueObjectId,
    },
    Admitted {
        frame: QueueObjectId,
    },
    /// Authoritative session deletion permits local cleanup only.
    Deleted,
}

impl QueueSendPhase {
    pub(crate) fn terminal(&self) -> bool {
        matches!(self, Self::Admitted { .. } | Self::Deleted)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum QueueEnqueuePhase {
    Capturing,
    Ready {
        payload: QueueObjectId,
        request: QueueJson,
    },
    Unknown {
        payload: QueueObjectId,
        request: QueueJson,
    },
    Accepted {
        payload: QueueObjectId,
        request: QueueJson,
        receipt: QueueJson,
    },
    /// One explicit server-delivery retry, saved before its authorization POST.
    RetryPrepared {
        payload: QueueObjectId,
        request: QueueJson,
        receipt: QueueJson,
        retry: QueueJson,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum QueueTakePhase {
    Requested,
    Unknown,
    Transferring(QueueTransfer),
    LocalOwned(QueueLocalTake),
    AcknowledgementUnknown(QueueLocalTake),
    Acknowledged {
        local: QueueLocalTake,
        receipt: QueueJson,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueuePrefixHandoff {
    pub(crate) receipt: QueueJson,
    pub(crate) items: Vec<QueueTransferItem>,
    pub(crate) composer: QueueObjectId,
    pub(crate) prompt: QueueObjectId,
    pub(crate) context_ids: Vec<String>,
    /// Earlier v8 prefix captures did not retain the composer's sealed index.
    #[serde(default)]
    pub(crate) composer_item: Option<QueueTransferItem>,
}

/// Native metadata derived from an already-validated transport result. Display
/// previews stay in the separate sealed indexes, never in this comparison record.
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PrefixReceipt {
    directory: String,
    message: String,
    steer: bool,
    composer_sha256: [u8; 32],
    composer_index_sha256: [u8; 32],
    items: Vec<PrefixReceiptItem>,
    context_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PrefixReceiptItem {
    item: String,
    payload_sha256: [u8; 32],
    index_sha256: [u8; 32],
}

impl QueuePrefixHandoff {
    /// Preview and reservation compare through this same revision-free capture.
    pub(crate) fn capture_receipt(prefix: &QueuePayloadPrefix) -> Result<QueueJson, JournalError> {
        let receipt = PrefixReceipt {
            directory: prefix.directory.clone(),
            message: prefix.id.to_string(),
            steer: prefix.steer,
            composer_sha256: prefix.composer.pointer().sha256(),
            composer_index_sha256: prefix.composer.index_hash(),
            items: prefix
                .items
                .iter()
                .map(|item| PrefixReceiptItem {
                    item: item.id.to_string(),
                    payload_sha256: item.payload.pointer().sha256(),
                    index_sha256: item.payload.index_hash(),
                })
                .collect(),
            context_ids: prefix.context_ids.iter().map(ToString::to_string).collect(),
        };
        QueueJson::new(
            serde_json::to_vec(&receipt)
                .map_err(|_| JournalError::Invalid("prefix receipt cannot be encoded"))?,
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum QueuePrefixPhase {
    Capturing,
    Previewed(QueuePrefixHandoff),
    ReservationUnknown(QueuePrefixHandoff),
    Reserved(QueuePrefixHandoff),
    Admitted(QueuePrefixHandoff),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum QueueAction {
    Enqueue {
        source: QueueSource,
        phase: QueueEnqueuePhase,
    },
    Take {
        purpose: QueueTakePurpose,
        request: QueueJson,
        phase: QueueTakePhase,
    },
    Prefix {
        message: String,
        source: QueueSource,
        request: QueueJson,
        phase: QueuePrefixPhase,
    },
    Send {
        message: String,
        source: QueueSource,
        delivery: Option<Delivery>,
        context_ids: Vec<String>,
        phase: QueueSendPhase,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueOperation {
    id: QueueOperationId,
    owner: QueueOwner,
    configuration: QueueConfiguration,
    /// Prepared files and source groups remain protected after the source is spooled.
    native_roots: Vec<String>,
    prepared_roots: Vec<String>,
    #[serde(default)]
    source_intent: Option<QueueSourceIntent>,
    #[serde(default)]
    source_seal: Option<QueueSourceSeal>,
    /// Absent in older v8 snapshots. It grants no implicit preparation authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preparation: Option<QueueComposerPreparation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    take_source_use: Option<QueueTakeSourceUse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_occurrences: Option<Vec<QueueSourceOccurrence>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    prefix_downloads: Vec<QueueTransferItem>,
    action: QueueAction,
}

impl QueueOperation {
    pub(crate) fn enqueue_with_edit(
        id: QueueOperationId,
        owner: QueueOwner,
        configuration: QueueConfiguration,
        capture: CompanionCapture,
        native_context_ids: Vec<String>,
        files: Vec<QueueNativeFileDescriptor>,
        edit: Option<ComposerEditRef>,
    ) -> Result<Self, JournalError> {
        if capture.native.owner != owner.draft_key()? {
            return Err(JournalError::Invalid(
                "compound enqueue belongs to another draft",
            ));
        }
        validate_native_capture(&capture.native, &native_context_ids, &files)?;
        let intent = NativeEnqueueIntent {
            kind: NativeEnqueueKind::Capture,
            directory: owner.directory.clone(),
            companions: capture.references,
            native_context_ids,
            files,
            edit,
        };
        let mut operation = Self {
            id,
            owner,
            configuration,
            native_roots: native_roots(&capture.native),
            prepared_roots: prepared_roots(&capture.native),
            source_intent: Some(QueueSourceIntent::Enqueue(intent)),
            source_seal: None,
            preparation: None,
            take_source_use: None,
            source_occurrences: None,
            prefix_downloads: Vec::new(),
            action: QueueAction::Enqueue {
                source: QueueSource::Inline(Box::new(capture.native)),
                phase: QueueEnqueuePhase::Capturing,
            },
        };
        operation.capture_source_seal()?;
        Ok(operation)
    }
    #[cfg(test)]
    pub(crate) fn enqueue(
        id: QueueOperationId,
        owner: QueueOwner,
        configuration: QueueConfiguration,
        source: QueueNativeSource,
    ) -> Result<Self, JournalError> {
        if source.owner != owner.draft_key()? {
            return Err(JournalError::Invalid(
                "queue source belongs to another draft",
            ));
        }
        let native_roots = native_roots(&source);
        let prepared_roots = prepared_roots(&source);
        Ok(Self {
            id,
            owner,
            configuration,
            native_roots,
            prepared_roots,
            source_intent: None,
            source_seal: None,
            preparation: None,
            take_source_use: None,
            source_occurrences: None,
            prefix_downloads: Vec::new(),
            action: QueueAction::Enqueue {
                source: QueueSource::Inline(Box::new(source)),
                phase: QueueEnqueuePhase::Capturing,
            },
        })
    }

    pub(crate) fn take(
        id: QueueOperationId,
        owner: QueueOwner,
        configuration: QueueConfiguration,
        purpose: QueueTakePurpose,
        request: QueueJson,
    ) -> Self {
        let take_source_use =
            matches!(&purpose, QueueTakePurpose::Edit).then_some(QueueTakeSourceUse::Unused);
        Self {
            id,
            owner,
            configuration,
            native_roots: Vec::new(),
            prepared_roots: Vec::new(),
            source_intent: None,
            source_seal: None,
            preparation: None,
            take_source_use,
            source_occurrences: None,
            prefix_downloads: Vec::new(),
            action: QueueAction::Take {
                purpose,
                request,
                phase: QueueTakePhase::Requested,
            },
        }
    }

    pub(crate) fn prefix(
        id: QueueOperationId,
        owner: QueueOwner,
        configuration: QueueConfiguration,
        message: MessageId,
        source: QueueNativeSource,
        request: QueueJson,
    ) -> Result<Self, JournalError> {
        if source.owner != owner.draft_key()? {
            return Err(JournalError::Invalid(
                "prefix source belongs to another draft",
            ));
        }
        let native_roots = native_roots(&source);
        let prepared_roots = prepared_roots(&source);
        let source_intent = if request.value()?.get("kind").is_some() {
            let intent: NativePrefixIntent = serde_json::from_slice(request.bytes())
                .map_err(|_| JournalError::Invalid("invalid native prefix capture intent"))?;
            validate_prefix_intent(&intent, &owner, message.as_str())?;
            validate_native_capture(&source, &intent.native_context_ids, &intent.files)?;
            Some(QueueSourceIntent::Prefix(intent))
        } else {
            validate_real_prefix_request(&request, &owner, message.as_str())?;
            None
        };
        let mut operation = Self {
            id,
            owner,
            configuration,
            native_roots,
            prepared_roots,
            source_intent,
            source_seal: None,
            preparation: None,
            take_source_use: None,
            source_occurrences: None,
            prefix_downloads: Vec::new(),
            action: QueueAction::Prefix {
                message: message.to_string(),
                source: QueueSource::Inline(Box::new(source)),
                request,
                phase: QueuePrefixPhase::Capturing,
            },
        };
        operation.capture_source_seal()?;
        Ok(operation)
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn local_send(
        id: QueueOperationId,
        owner: QueueOwner,
        configuration: QueueConfiguration,
        message: MessageId,
        capture: CompanionCapture,
        delivery: Option<Delivery>,
        native_context_ids: Vec<String>,
        files: Vec<QueueNativeFileDescriptor>,
    ) -> Result<Self, JournalError> {
        Self::local_send_with_edit(
            id,
            owner,
            configuration,
            message,
            capture,
            delivery,
            native_context_ids,
            files,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn local_send_with_edit(
        id: QueueOperationId,
        owner: QueueOwner,
        configuration: QueueConfiguration,
        message: MessageId,
        capture: CompanionCapture,
        delivery: Option<Delivery>,
        native_context_ids: Vec<String>,
        files: Vec<QueueNativeFileDescriptor>,
        edit: Option<ComposerEditRef>,
    ) -> Result<Self, JournalError> {
        if capture.native.owner != owner.draft_key()? {
            return Err(JournalError::Invalid(
                "local send source belongs to another draft",
            ));
        }
        let intent = NativeSendIntent {
            kind: NativeSendKind::Capture,
            directory: owner.directory.clone(),
            id: message.to_string(),
            delivery,
            companions: capture.references,
            native_context_ids,
            files,
            edit,
        };
        validate_native_capture(&capture.native, &intent.native_context_ids, &intent.files)?;
        let mut operation = Self {
            id,
            owner,
            configuration,
            native_roots: native_roots(&capture.native),
            prepared_roots: prepared_roots(&capture.native),
            source_seal: None,
            preparation: None,
            take_source_use: None,
            source_occurrences: None,
            prefix_downloads: Vec::new(),
            action: QueueAction::Send {
                message: message.to_string(),
                source: QueueSource::Inline(Box::new(capture.native)),
                delivery,
                context_ids: intent.native_context_ids.clone(),
                phase: QueueSendPhase::Capturing,
            },
            source_intent: Some(QueueSourceIntent::Send(intent)),
        };
        operation.capture_source_seal()?;
        Ok(operation)
    }

    pub(crate) fn id(&self) -> &QueueOperationId {
        &self.id
    }
    pub(crate) fn owner(&self) -> &QueueOwner {
        &self.owner
    }
    pub(crate) fn configuration(&self) -> &QueueConfiguration {
        &self.configuration
    }
    pub(crate) fn action(&self) -> &QueueAction {
        &self.action
    }
    pub(crate) fn has_compound_source(&self) -> bool {
        self.source_intent.is_some()
    }
    pub(crate) fn composer_preparation(&self) -> Option<&QueueComposerPreparation> {
        self.preparation.as_ref()
    }
    pub(crate) fn source_sha256(&self) -> Option<&[u8; 32]> {
        self.source_seal.as_ref().map(|seal| &seal.sha256)
    }
    #[cfg(test)]
    pub(crate) fn source_occurrences(&self) -> Option<&[QueueSourceOccurrence]> {
        self.source_occurrences.as_deref()
    }
    pub(crate) fn take_source_use(&self) -> Option<&QueueTakeSourceUse> {
        self.take_source_use.as_ref()
    }

    /// Marks the unified preparation protocol in the original admission snapshot.
    /// An empty marker claims no input or output bytes and distinguishes a new
    /// native-only capture from older operations after an interrupted first read.
    pub(crate) fn begin_composer_preparation(
        &mut self,
        generation: u64,
        revision: u64,
    ) -> Result<(), JournalError> {
        if !capturing_composer(self)
            || self.preparation.is_some()
            || generation == 0
            || generation == u64::MAX
            || revision == 0
            || revision == u64::MAX
        {
            return Err(JournalError::Invalid(
                "invalid initial composer preparation marker",
            ));
        }
        let source = *self.source_sha256().ok_or(JournalError::Invalid(
            "composer preparation has no original source seal",
        ))?;
        self.preparation = Some(QueueComposerPreparation::new(
            &self.owner.draft_key()?,
            source,
            generation,
            revision,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        Ok(())
    }

    /// Reads the original typed local source before its spool publication.
    pub(crate) fn local_send_capture(&self) -> Result<SendSourceCapture, JournalError> {
        let QueueAction::Send {
            source: QueueSource::Inline(native),
            ..
        } = &self.action
        else {
            return Err(JournalError::Invalid("local send has no inline source"));
        };
        let Some(QueueSourceIntent::Send(intent)) = &self.source_intent else {
            return Err(JournalError::Invalid(
                "local send has no typed capture intent",
            ));
        };
        Ok(SendSourceCapture {
            version: 1,
            composer: CompanionCapture {
                native: native.as_ref().clone(),
                references: intent.companions.clone(),
            },
            intent: intent.clone(),
        })
    }

    /// Reads an original compound enqueue before its source object is published.
    pub(crate) fn enqueue_capture(&self) -> Result<EnqueueSourceCapture, JournalError> {
        let QueueAction::Enqueue {
            source: QueueSource::Inline(native),
            ..
        } = &self.action
        else {
            return Err(JournalError::Invalid("enqueue has no inline source"));
        };
        let Some(QueueSourceIntent::Enqueue(intent)) = &self.source_intent else {
            return Err(JournalError::Invalid("enqueue has no typed capture intent"));
        };
        Ok(EnqueueSourceCapture {
            version: 1,
            composer: CompanionCapture {
                native: native.as_ref().clone(),
                references: intent.companions.clone(),
            },
            intent: intent.clone(),
        })
    }
    pub(crate) fn dependencies(&self) -> &[DraftCompanionRef] {
        self.source_intent
            .as_ref()
            .map_or(&[], QueueSourceIntent::companions)
    }
    pub(crate) fn composer_edit(&self) -> Option<&ComposerEditRef> {
        self.source_intent
            .as_ref()
            .and_then(QueueSourceIntent::edit)
    }

    pub(crate) fn source(&self) -> Option<&QueueSource> {
        match &self.action {
            QueueAction::Enqueue { source, .. }
            | QueueAction::Prefix { source, .. }
            | QueueAction::Send { source, .. } => Some(source),
            QueueAction::Take { .. } => None,
        }
    }

    pub(crate) fn compound_source_bytes(&self) -> Result<Vec<u8>, JournalError> {
        #[derive(Serialize)]
        struct Composer<'a> {
            native: &'a QueueNativeSource,
            references: &'a [DraftCompanionRef],
        }
        #[derive(Serialize)]
        struct Capture<'a, T> {
            version: u32,
            composer: Composer<'a>,
            intent: &'a T,
        }

        let Some(QueueSource::Inline(native)) = self.source() else {
            return Err(JournalError::Invalid(
                "compound source is already published",
            ));
        };
        let bytes = match &self.source_intent {
            Some(QueueSourceIntent::Enqueue(intent)) => serde_json::to_vec(&Capture {
                version: 1,
                composer: Composer {
                    native,
                    references: &intent.companions,
                },
                intent,
            }),
            Some(QueueSourceIntent::Prefix(intent)) => serde_json::to_vec(&Capture {
                version: 1,
                composer: Composer {
                    native,
                    references: &intent.companions,
                },
                intent,
            }),
            Some(QueueSourceIntent::Send(intent)) => serde_json::to_vec(&Capture {
                version: 1,
                composer: Composer {
                    native,
                    references: &intent.companions,
                },
                intent,
            }),
            None => {
                return Err(JournalError::Invalid(
                    "operation has no typed compound source",
                ));
            }
        }
        .map_err(|_| JournalError::Invalid("compound queue source cannot be encoded"))?;
        if bytes.len() as u64 > crate::queue_capture::QUEUE_CAPTURE_BYTES {
            return Err(JournalError::Invalid(
                "compound queue source exceeds its capture limit",
            ));
        }
        Ok(bytes)
    }

    fn capture_source_seal(&mut self) -> Result<(), JournalError> {
        if self.source_intent.is_some() {
            let bytes = self.compound_source_bytes()?;
            self.source_seal = Some(QueueSourceSeal {
                byte_len: bytes.len() as u64,
                sha256: sha256(&bytes),
            });
        }
        Ok(())
    }
}

fn native_roots(source: &QueueNativeSource) -> Vec<String> {
    let mut roots = Vec::new();
    for file in &source.draft.content.files {
        if matches!(&file.content, DraftFileContent::Prepared)
            && !roots.contains(&file.id.to_string())
        {
            roots.push(file.id.to_string());
        }
        if let Some(group) = &file.source_document_id
            && !roots.contains(&group.to_string())
        {
            roots.push(group.to_string());
        }
    }
    roots
}

fn prepared_roots(source: &QueueNativeSource) -> Vec<String> {
    source
        .draft
        .content
        .files
        .iter()
        .filter(|file| matches!(&file.content, DraftFileContent::Prepared))
        .map(|file| file.id.to_string())
        .collect()
}

fn canonical_retirement_files(
    mut files: Vec<QueueNativeFileDescriptor>,
) -> Result<Vec<QueueNativeFileDescriptor>, JournalError> {
    if files.len() > MAX_ENTRIES {
        return Err(JournalError::Invalid("too many cleanup file descriptors"));
    }
    for file in &files {
        BlobId::parse(&file.id)
            .map_err(|_| JournalError::Invalid("invalid cleanup attachment identity"))?;
        if file.byte_len > crate::queue_capture::QUEUE_CHUNK_BYTES as u64 {
            return Err(JournalError::Invalid(
                "cleanup attachment exceeds its prepared-byte bound",
            ));
        }
    }
    files.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    if files
        .windows(2)
        .any(|pair| pair[0].id == pair[1].id && pair[0] != pair[1])
    {
        return Err(JournalError::Invalid(
            "cleanup attachment identity has conflicting descriptors",
        ));
    }
    files.dedup();
    Ok(files)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum QueueObjectProgress {
    Writing { durable_ordinals: Vec<usize> },
    Verified,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueObject {
    id: QueueObjectId,
    operation: QueueOperationId,
    kind: QueueObjectKind,
    generation: u64,
    reference: QueueSpoolRef,
    progress: QueueObjectProgress,
}

impl QueueObject {
    pub(crate) fn planned(
        id: QueueObjectId,
        operation: QueueOperationId,
        kind: QueueObjectKind,
        generation: u64,
        reference: QueueSpoolRef,
    ) -> Result<Self, JournalError> {
        reference.validate(kind)?;
        if generation == 0 {
            return Err(JournalError::Invalid("zero queue preparation generation"));
        }
        Ok(Self {
            id,
            operation,
            kind,
            generation,
            reference,
            progress: QueueObjectProgress::Writing {
                durable_ordinals: Vec::new(),
            },
        })
    }

    pub(crate) fn id(&self) -> &QueueObjectId {
        &self.id
    }
    pub(crate) fn operation(&self) -> &QueueOperationId {
        &self.operation
    }
    pub(crate) fn kind(&self) -> QueueObjectKind {
        self.kind
    }
    pub(crate) fn reference(&self) -> &QueueSpoolRef {
        &self.reference
    }
    pub(crate) fn verified(&self) -> bool {
        matches!(self.progress, QueueObjectProgress::Verified)
    }

    pub(crate) fn hold(&self, ordinal: usize) -> Option<QueueChunkHold> {
        if self.chunk_stored(ordinal) {
            return None;
        }
        Some(QueueChunkHold {
            operation: self.operation.clone(),
            object: self.id.clone(),
            generation: self.generation,
            ordinal,
            chunk: self.reference.chunks().get(ordinal)?.clone(),
        })
    }

    fn chunk_stored(&self, ordinal: usize) -> bool {
        match &self.progress {
            QueueObjectProgress::Writing { durable_ordinals } => {
                durable_ordinals.contains(&ordinal)
            }
            QueueObjectProgress::Verified => ordinal < self.reference.chunks().len(),
        }
    }

    pub(crate) fn ready_to_verify(&self) -> bool {
        (0..self.reference.chunks().len()).all(|ordinal| self.chunk_stored(ordinal))
    }

    pub(crate) fn verification(&self) -> QueueVerification {
        QueueVerification {
            operation: self.operation.clone(),
            object: self.id.clone(),
            generation: self.generation,
            reference: self.reference.clone(),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueStorage {
    operations: Vec<QueueOperation>,
    objects: Vec<QueueObject>,
    retired: Vec<String>,
    #[serde(default)]
    draft_companions: Vec<DraftCompanionRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    composer_edits: Vec<ComposerEditRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cleared_takes: Vec<QueueOperationId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    local_send_retirements: Vec<QueueLocalSendRetirement>,
    #[serde(skip)]
    captured_sources: Vec<Weak<CapturedComposerSource>>,
}

/// The worker copies only byte authority, never an expanded inline source draft.
#[derive(Clone, Default)]
pub(crate) struct QueueByteAuthority {
    pub(crate) objects: Vec<QueueObject>,
    pub(crate) holds: Vec<QueueChunkHold>,
    pub(crate) stored: Vec<BlobDescriptor>,
    pub(crate) retired: Vec<BlobId>,
    pub(crate) native_roots: HashSet<String>,
    pub(crate) acknowledgements: Vec<QueueAcknowledgementAuthority>,
    consumer_edits: Vec<QueueConsumerEditBinding>,
}

impl QueueByteAuthority {
    pub(crate) fn permits_consumer_edit(&self, binding: &QueueConsumerEditBinding) -> bool {
        self.consumer_edits.contains(binding)
    }
}

#[derive(Clone)]
pub(crate) struct QueueAcknowledgementAuthority {
    pub(crate) operation: QueueOperationId,
    pub(crate) directory: String,
    pub(crate) request: QueueJson,
    pub(crate) receipt: QueueJson,
    pub(crate) objects: Vec<QueueObjectId>,
    pub(crate) items: Vec<QueueTransferItem>,
}

impl QueueStorage {
    pub(crate) fn capture_composer_source(
        &mut self,
        consumer: QueueOperationId,
        owner: DraftKey,
        companions: Vec<DraftCompanionRef>,
        edit: Option<ComposerEditRef>,
        native_roots: Vec<AttachmentId>,
    ) -> Result<Arc<CapturedComposerSource>, JournalError> {
        if !ordered_references(&self.draft_companions(&owner), &companions)
            || edit.as_ref().is_some_and(|edit| {
                edit.companions != companions || !self.owns_composer_edit_write(&owner, edit)
            })
        {
            return Err(JournalError::Invalid(
                "submission has no exact current capture authority",
            ));
        }
        for reference in &companions {
            let take = self
                .operation(&reference.operation)
                .ok_or(JournalError::Invalid("captured Take is missing"))?;
            if take.owner.draft_key()? != owner
                || !matches!(take.action(), QueueAction::Take {
                purpose: QueueTakePurpose::Edit, phase: QueueTakePhase::Acknowledged { local: QueueLocalTake {
                    destination: QueueDestination::Draft { companion }, .. }, .. }, .. } if companion == &reference.companion)
            {
                return Err(JournalError::Invalid(
                    "submission lost genuine acknowledged source authority",
                ));
            }
            self.require_object(
                take,
                &reference.companion,
                QueueObjectKind::NativeCompanion,
                true,
            )?;
        }
        self.captured_sources
            .retain(|source| source.strong_count() > 0);
        let source = Arc::new(CapturedComposerSource {
            consumer,
            owner,
            companions,
            edit,
            native_roots,
        });
        self.captured_sources.push(Arc::downgrade(&source));
        Ok(source)
    }

    /// Admit exact provenance and every pre-put hold in one snapshot. The source
    /// verifier must inspect the captured source/edit bytes, including each input
    /// mention, before returning. The caller durably saves before any byte put.
    pub(crate) fn plan_composer_preparation(
        &mut self,
        id: &QueueOperationId,
        preparation: QueueComposerPreparation,
        attachments: &AttachmentCatalog,
        preparations: &mut Vec<PreparationHold>,
        verify_sources: impl FnOnce(
            &QueueOperation,
            &QueueComposerPreparation,
        ) -> Result<(), JournalError>,
        preflight: impl FnOnce(&Self, &[PreparationHold]) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let index = self
            .operations
            .iter()
            .position(|operation| &operation.id == id)
            .ok_or(JournalError::Invalid("missing composer preparation owner"))?;
        let operation = &self.operations[index];
        if !capturing_composer(operation)
            || preparation.inputs_object.is_some()
            || operation
                .preparation
                .as_ref()
                .is_some_and(|old| old.inputs_object.is_some())
        {
            return Err(JournalError::Invalid(
                "composer preparation is sealed or no longer capturing",
            ));
        }
        let previous = operation.preparation.clone();
        let existing_files = if let Some(old) = &previous {
            if preparation.owner()? != old.owner()?
                || preparation.source_sha256 != old.source_sha256
                || preparation.generation != old.generation
                || preparation.revision != old.revision
                || !preparation.sources.starts_with(&old.sources)
                || !preparation.files.starts_with(&old.files)
                || !preparation
                    .derived_context_ids
                    .starts_with(&old.derived_context_ids)
            {
                return Err(JournalError::Invalid(
                    "composer preparation extension changed an existing record",
                ));
            }
            old.files.len()
        } else {
            0
        };
        if preparation.files[existing_files..]
            .iter()
            .any(|file| file.state != QueueDerivedFileState::Proposed)
        {
            return Err(JournalError::Invalid(
                "new derived files require pre-put proposals",
            ));
        }
        if preparation.files[existing_files..].iter().any(|new| {
            preparation.files[..existing_files].iter().any(|old| {
                old.source == new.source
                    && old.mention.start == new.mention.start
                    && old.mention.end == new.mention.end
            })
        }) {
            return Err(JournalError::Invalid(
                "an existing derived mention bundle cannot gain new outputs",
            ));
        }
        self.validate_source_seal(operation)?;
        if self.objects.iter().any(|object| {
            object.operation == operation.id
                && !operation.source_seal.as_ref().is_some_and(|seal| {
                    object.kind == QueueObjectKind::NativeCompanion
                        && object.reference.total_bytes() == seal.byte_len
                        && object.reference.sha256() == &seal.sha256
                })
                && !operation
                    .prefix_downloads
                    .iter()
                    .any(|item| item.payload == object.id)
        }) {
            return Err(JournalError::Invalid(
                "composer inputs are already being prepared",
            ));
        }
        let owner = operation.owner.draft_key()?;
        let holds = preparation.files[existing_files..]
            .iter()
            .map(|file| file.hold(&owner))
            .collect::<Result<Vec<_>, _>>()?;
        if preparations.len().saturating_add(holds.len()) > MAX_ENTRIES
            || holds.iter().any(|hold| {
                attachments.contains_key(&hold.id)
                    || preparations.iter().any(|old| old.id == hold.id)
            })
        {
            return Err(JournalError::Invalid(
                "derived attachment reuses an existing byte owner",
            ));
        }
        let previous_len = preparations.len();
        preparations.extend(holds);
        self.operations[index].preparation = Some(preparation);
        let previous_contexts = match self.source_context_ids(&self.operations[index]) {
            Ok(contexts) => match &mut self.operations[index].action {
                QueueAction::Send { context_ids, .. } => {
                    Some(std::mem::replace(context_ids, contexts))
                }
                _ => None,
            },
            Err(error) => {
                self.operations[index].preparation = previous;
                preparations.truncate(previous_len);
                return Err(error);
            }
        };
        let result = (|| {
            self.validate_composer_preparation(
                &self.operations[index],
                attachments,
                preparations,
                &[],
            )?;
            self.validate_context_reservations()?;
            let operation = &self.operations[index];
            verify_sources(
                operation,
                operation
                    .preparation
                    .as_ref()
                    .expect("candidate retains its preparation"),
            )?;
            preflight(self, preparations)
        })();
        if let Err(error) = result {
            self.operations[index].preparation = previous;
            preparations.truncate(previous_len);
            if let (Some(previous), QueueAction::Send { context_ids, .. }) =
                (previous_contexts, &mut self.operations[index].action)
            {
                *context_ids = previous;
            }
            return Err(error);
        }
        Ok(())
    }

    /// Submit the full existing prefix plus a new source/file/context tail. Clone
    /// the current preparation, append the complete new mention's output bundle,
    /// and keep every earlier status and identity unchanged. Exact retries add no
    /// holds. A failed later read leaves earlier published bundles with this owner.
    #[cfg(test)]
    pub(crate) fn extend_composer_preparation(
        &mut self,
        id: &QueueOperationId,
        extended: QueueComposerPreparation,
        attachments: &AttachmentCatalog,
        preparations: &mut Vec<PreparationHold>,
        verify_sources: impl FnOnce(
            &QueueOperation,
            &QueueComposerPreparation,
        ) -> Result<(), JournalError>,
        preflight: impl FnOnce(&Self, &[PreparationHold]) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        if self
            .operation(id)
            .is_none_or(|operation| operation.preparation.is_none())
        {
            return Err(JournalError::Invalid(
                "composer extension has no original plan",
            ));
        }
        self.plan_composer_preparation(
            id,
            extended,
            attachments,
            preparations,
            verify_sources,
            preflight,
        )
    }

    /// The exact durable put receipt publishes catalog custody. The caller saves
    /// this transition before publishing a prepared-input object.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn publish_composer_derived_file(
        &mut self,
        id: &QueueOperationId,
        generation: u64,
        file_ordinal: usize,
        receipt: &BlobReceipt,
        attachments: &mut AttachmentCatalog,
        preparations: &mut Vec<PreparationHold>,
        preflight: impl FnOnce(
            &Self,
            &AttachmentCatalog,
            &[PreparationHold],
        ) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let index = self
            .operations
            .iter()
            .position(|operation| &operation.id == id)
            .ok_or(JournalError::Invalid("missing derived file owner"))?;
        let operation = &self.operations[index];
        let preparation = operation.preparation.as_ref().ok_or(JournalError::Invalid(
            "derived file has no admitted preparation",
        ))?;
        let file = preparation
            .files
            .get(file_ordinal)
            .ok_or(JournalError::Invalid("missing derived file ordinal"))?;
        let hold = file.hold(&operation.owner.draft_key()?)?;
        if !capturing_composer(operation)
            || preparation.generation != generation
            || preparation.inputs_object.is_some()
            || file.state != QueueDerivedFileState::Proposed
            || receipt.commit != SpoolCommit::Durable
            || receipt.descriptor != file.descriptor()?
            || attachments.contains_key(&hold.id)
        {
            return Err(JournalError::Invalid(
                "derived file receipt differs from its immutable plan",
            ));
        }
        let hold_index =
            preparations
                .iter()
                .position(|saved| saved == &hold)
                .ok_or(JournalError::Invalid(
                    "derived file lost its exact pre-put hold",
                ))?;
        let removed = preparations.remove(hold_index);
        attachments.insert(
            hold.id.clone(),
            PreparedAttachment {
                blob: receipt.descriptor,
                staging: None,
            },
        );
        self.operations[index]
            .preparation
            .as_mut()
            .expect("checked preparation")
            .files[file_ordinal]
            .state = QueueDerivedFileState::Cataloged;
        let result = (|| {
            self.validate_composer_preparation(
                &self.operations[index],
                attachments,
                preparations,
                &[],
            )?;
            preflight(self, attachments, preparations)
        })();
        if let Err(error) = result {
            self.operations[index]
                .preparation
                .as_mut()
                .expect("publication retains its preparation")
                .files[file_ordinal]
                .state = QueueDerivedFileState::Proposed;
            attachments.remove(&hold.id);
            preparations.insert(hold_index, removed);
            return Err(error);
        }
        Ok(())
    }

    /// Bind one verified immutable prepared-input capture. The verifier compares
    /// its decoded bytes with the plan, source, configuration and full context
    /// order. It must read through the committed byte owner, never current typing.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn seal_composer_inputs(
        &mut self,
        id: &QueueOperationId,
        generation: u64,
        revision: u64,
        inputs: QueueObjectId,
        attachments: &AttachmentCatalog,
        preparations: &[PreparationHold],
        verify_inputs: impl FnOnce(
            &QueueOperation,
            &QueueComposerPreparation,
            &QueueObject,
        ) -> Result<(), JournalError>,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let index = self
            .operations
            .iter()
            .position(|operation| &operation.id == id)
            .ok_or(JournalError::Invalid("missing prepared input owner"))?;
        let operation = &self.operations[index];
        let preparation = operation
            .preparation
            .as_ref()
            .ok_or(JournalError::Invalid("inputs have no preparation owner"))?;
        if matches!(
            operation.action(),
            QueueAction::Send {
                phase: QueueSendPhase::Deleted,
                ..
            }
        ) || preparation.generation != generation
            || preparation.revision != revision
            || preparation
                .inputs_object
                .as_ref()
                .is_some_and(|previous| previous != &inputs)
            || preparation
                .files
                .iter()
                .any(|file| file.state != QueueDerivedFileState::Cataloged)
        {
            return Err(JournalError::Invalid(
                "prepared inputs changed their plan or lack cataloged files",
            ));
        }
        self.validate_composer_preparation(operation, attachments, preparations, &[])?;
        let object =
            self.require_object(operation, &inputs, QueueObjectKind::NativeCompanion, true)?;
        if object.generation != generation
            || matches!(operation.source(), Some(QueueSource::Companion(source)) if source == &inputs)
            || operation
                .composer_edit()
                .is_some_and(|edit| edit.object == inputs)
        {
            return Err(JournalError::Invalid(
                "prepared inputs alias original source bytes or another generation",
            ));
        }
        verify_inputs(operation, preparation, object)?;
        if preparation.inputs_object.is_some() {
            return preflight(self);
        }
        if !capturing_composer(operation) {
            return Err(JournalError::Invalid(
                "prepared inputs require capturing ownership",
            ));
        }
        self.operations[index]
            .preparation
            .as_mut()
            .expect("checked preparation")
            .inputs_object = Some(inputs);
        if let Err(error) = preflight(self) {
            self.operations[index]
                .preparation
                .as_mut()
                .expect("publication retains its preparation")
                .inputs_object = None;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn composer_context_ids(
        &self,
        id: &QueueOperationId,
    ) -> Result<Vec<String>, JournalError> {
        self.source_context_ids(
            self.operation(id)
                .ok_or(JournalError::Invalid("missing composer context owner"))?,
        )
    }

    pub(crate) fn prefix_downloads(&self, id: &QueueOperationId) -> Option<&[QueueTransferItem]> {
        let operation = self.operation(id)?;
        matches!(operation.action(), QueueAction::Prefix { .. })
            .then_some(operation.prefix_downloads.as_slice())
    }

    /// Save complete original download bindings before any attachment put.
    /// Repeated preparation must name exactly the same payloads and local labels.
    pub(crate) fn prepare_prefix_downloads(
        &mut self,
        id: &QueueOperationId,
        items: Vec<QueueTransferItem>,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let index = self
            .operations
            .iter()
            .position(|operation| &operation.id == id)
            .ok_or(JournalError::Invalid("missing prefix download owner"))?;
        let operation = &self.operations[index];
        if !matches!(
            operation.action(),
            QueueAction::Prefix {
                phase: QueuePrefixPhase::Capturing,
                ..
            }
        ) {
            return Err(JournalError::Invalid(
                "prefix downloads require capturing ownership",
            ));
        }
        if !operation.prefix_downloads.is_empty() {
            return if operation.prefix_downloads == items {
                Ok(())
            } else {
                Err(JournalError::Invalid(
                    "prefix download bindings are immutable",
                ))
            };
        }
        self.validate_prefix_downloads(operation, &items)?;
        self.operations[index].prefix_downloads = items;
        if let Err(error) = preflight(self) {
            self.operations[index].prefix_downloads.clear();
            return Err(error);
        }
        Ok(())
    }

    fn validate_prefix_downloads(
        &self,
        operation: &QueueOperation,
        items: &[QueueTransferItem],
    ) -> Result<(), JournalError> {
        let QueueAction::Prefix { request, .. } = operation.action() else {
            return Err(JournalError::Invalid(
                "downloads belong to a non-prefix operation",
            ));
        };
        let ids = match &operation.source_intent {
            Some(QueueSourceIntent::Prefix(intent)) => intent.item_ids.clone(),
            _ => request
                .value()?
                .get("itemIds")
                .and_then(serde_json::Value::as_array)
                .ok_or(JournalError::Invalid(
                    "prefix downloads have no original item order",
                ))?
                .iter()
                .map(|id| {
                    id.as_str()
                        .map(str::to_owned)
                        .ok_or(JournalError::Invalid("invalid prefix download identity"))
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        if items.is_empty()
            || items.len() > 20
            || items.iter().map(|item| &item.item).ne(ids.iter())
        {
            return Err(JournalError::Invalid(
                "prefix downloads changed their complete item order",
            ));
        }
        let mut local_ids = HashSet::new();
        for item in items {
            let indexed = QueueIndexedPayload::from_body(
                item.index.bytes(),
                operation.owner.directory(),
                sha256,
            )
            .map_err(|_| JournalError::Invalid("invalid original prefix download index"))?;
            let object =
                self.require_object(operation, &item.payload, QueueObjectKind::Payload, true)?;
            if object.reference.total_bytes() != indexed.pointer().total_bytes()
                || object.reference.sha256() != &indexed.pointer().sha256()
            {
                return Err(JournalError::Invalid(
                    "prefix download differs from its sealed payload",
                ));
            }
            let mut manifest = HashSet::new();
            let mut legacy = HashSet::new();
            for file in &item.files {
                file.descriptor()?;
                if !local_ids.insert(file.local()) {
                    return Err(JournalError::Invalid(
                        "prefix downloads reuse a local file label",
                    ));
                }
                match file {
                    QueueFileBinding::Manifest { blob, .. }
                        if indexed.index().attachment_blob_ids.contains(blob)
                            && manifest.insert(blob) => {}
                    QueueFileBinding::Legacy { ordinal, .. }
                        if indexed
                            .index()
                            .legacy_attachment_ids
                            .as_ref()
                            .is_some_and(|ids| *ordinal < ids.len())
                            && legacy.insert(ordinal) => {}
                    _ => {
                        return Err(JournalError::Invalid(
                            "prefix download file differs from its original index",
                        ));
                    }
                }
            }
            if manifest.len() != indexed.index().attachment_blob_ids.len()
                || legacy.len()
                    != indexed
                        .index()
                        .legacy_attachment_ids
                        .as_ref()
                        .map_or(0, Vec::len)
            {
                return Err(JournalError::Invalid(
                    "prefix download omits an original file binding",
                ));
            }
        }
        Ok(())
    }

    /// The caller has definite evidence that preview failed before reservation.
    /// This transition cannot abandon any phase that may own a server reservation.
    pub(crate) fn abandon_prefix(
        &mut self,
        id: &QueueOperationId,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing abandoned prefix owner"))?;
        if !matches!(
            operation.action(),
            QueueAction::Prefix {
                phase: QueuePrefixPhase::Capturing,
                ..
            }
        ) {
            return Err(JournalError::Invalid(
                "only a definitely unreserved capturing prefix can be abandoned",
            ));
        }
        self.remove_abandoned_prefix(id, preflight)
    }

    /// The server definitely refused this previewed prefix's reservation, so
    /// it owns none. A reserved or admitted prefix can never take this path.
    pub(crate) fn abandon_refused_prefix(
        &mut self,
        id: &QueueOperationId,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing abandoned prefix owner"))?;
        if !matches!(
            operation.action(),
            QueueAction::Prefix {
                phase: QueuePrefixPhase::Capturing
                    | QueuePrefixPhase::Previewed(_)
                    | QueuePrefixPhase::ReservationUnknown(_),
                ..
            }
        ) {
            return Err(JournalError::Invalid(
                "only an unreserved prefix can be abandoned",
            ));
        }
        self.remove_abandoned_prefix(id, preflight)
    }

    fn remove_abandoned_prefix(
        &mut self,
        id: &QueueOperationId,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let mut candidate = self.clone();
        for object in self.objects.iter().filter(|object| &object.operation == id) {
            for chunk in object.reference.chunks() {
                candidate
                    .retired
                    .push(chunk.descriptor()?.id().as_str().to_owned());
            }
        }
        candidate.operations.retain(|operation| &operation.id != id);
        candidate.objects.retain(|object| &object.operation != id);
        preflight(&candidate)?;
        *self = candidate;
        Ok(())
    }
    pub(crate) fn composer_edit(&self, owner: &DraftKey) -> Option<&ComposerEditRef> {
        self.composer_edits
            .iter()
            .find(|record| {
                record
                    .owner
                    .clone()
                    .into_key()
                    .as_ref()
                    .is_ok_and(|key| key == owner)
            })
            .and_then(|record| record.current.as_ref())
    }

    pub(crate) fn owns_composer_edit_write(
        &self,
        owner: &DraftKey,
        edit: &ComposerEditRef,
    ) -> bool {
        self.composer_edits.iter().any(|record| {
            record
                .owner
                .clone()
                .into_key()
                .as_ref()
                .is_ok_and(|key| key == owner)
                && record.versions.contains(edit)
        }) && self.validate_composer_edit(owner, edit, false).is_ok()
    }

    fn retains_pending_consumer_edit(&self, operation: &QueueOperation) -> bool {
        let Some(edit) = operation.composer_edit() else {
            return false;
        };
        let Some(preparation) = operation.composer_preparation() else {
            return false;
        };
        (capturing_composer(operation)
            || matches!(
                operation.action(),
                QueueAction::Send {
                    phase: QueueSendPhase::Deleted,
                    ..
                }
            ))
            && matches!(operation.source(), Some(QueueSource::Inline(_)))
            && preparation.generation == edit.generation
            && preparation.revision == edit.revision
            && preparation.sources.is_empty()
            && preparation.files.is_empty()
            && preparation.derived_context_ids.is_empty()
            && preparation.inputs_object.is_none()
            && operation.source_sha256() == Some(&preparation.source_sha256)
            && preparation.owner().as_ref().is_ok_and(|owner| {
                operation
                    .owner
                    .draft_key()
                    .as_ref()
                    .is_ok_and(|captured| captured == owner)
            })
    }

    pub(crate) fn consumer_edit_binding(
        &self,
        consumer: &QueueOperationId,
    ) -> Result<QueueConsumerEditBinding, JournalError> {
        let operation = self
            .operation(consumer)
            .ok_or(JournalError::Invalid("missing consuming edit owner"))?;
        if !capturing_composer(operation) || !self.retains_pending_consumer_edit(operation) {
            return Err(JournalError::Invalid(
                "consumer grants no pending edit write authority",
            ));
        }
        self.validate_source_seal(operation)?;
        self.validate_dependencies(operation)?;
        self.occurrence_context_ids(operation)?;
        let edit = operation
            .composer_edit()
            .expect("pending consumer has an exact edit");
        if edit.companions.as_slice() != operation.dependencies()
            || !self.owns_composer_edit_write(&operation.owner.draft_key()?, edit)
        {
            return Err(JournalError::Invalid(
                "consumer changed its exact pending edit plan",
            ));
        }
        Ok(QueueConsumerEditBinding {
            consumer: consumer.clone(),
            source_sha256: *operation.source_sha256().ok_or(JournalError::Invalid(
                "consumer lost its original source seal",
            ))?,
            edit: Arc::new(edit.clone()),
        })
    }

    pub(crate) fn record_consumer_edit_chunk(
        &mut self,
        binding: &QueueConsumerEditBinding,
        hold: &QueueChunkHold,
        receipt: &BlobReceipt,
    ) -> Result<(), JournalError> {
        if self.consumer_edit_binding(binding.consumer())?.ne(binding)
            || hold.operation != binding.edit.operation
            || hold.object != binding.edit.object
            || hold.generation != binding.edit.generation
        {
            return Err(JournalError::Invalid(
                "late chunk lost its exact consuming edit authority",
            ));
        }
        self.record_chunk(hold, receipt)
    }

    pub(crate) fn record_consumer_edit_verified(
        &mut self,
        proof: &QueueConsumerEditVerifiedCapture,
    ) -> Result<(), JournalError> {
        let binding = proof.binding();
        let verification = proof.verification();
        if self.consumer_edit_binding(binding.consumer())?.ne(binding)
            || !binding.matches_verification(verification)
        {
            return Err(JournalError::Invalid(
                "late verification lost its exact consuming edit authority",
            ));
        }
        let object = self
            .objects
            .iter_mut()
            .find(|object| object.id == verification.object)
            .ok_or(JournalError::Invalid(
                "missing consumer edit verification object",
            ))?;
        if !object.ready_to_verify() || object.verification() != *verification {
            return Err(JournalError::Invalid(
                "consumer edit verification changed its committed plan",
            ));
        }
        object.progress = QueueObjectProgress::Verified;
        Ok(())
    }

    pub(crate) fn consume_composer_capture(
        &mut self,
        consumer: &QueueOperationId,
        owner: &DraftKey,
        captured: &[DraftCompanionRef],
        edit: Option<&ComposerEditRef>,
        consumption: ComposerConsumption,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(consumer)
            .ok_or(JournalError::Invalid("missing exact composer consumer"))?;
        let consumable = matches!(
            operation.action(),
            QueueAction::Send {
                phase: QueueSendPhase::Capturing | QueueSendPhase::Prepared { .. },
                ..
            } | QueueAction::Enqueue {
                phase: QueueEnqueuePhase::Capturing | QueueEnqueuePhase::Ready { .. },
                ..
            } | QueueAction::Prefix {
                phase: QueuePrefixPhase::Capturing | QueuePrefixPhase::Previewed(_),
                ..
            }
        );
        if operation.owner.draft_key()?.ne(owner)
            || operation.dependencies() != captured
            || operation.composer_edit() != edit
            || !matches!(operation.source(), Some(QueueSource::Companion(_)))
            || !consumable
        {
            return Err(JournalError::Invalid(
                "compound composer has no exact published consumer",
            ));
        }
        self.validate_source_seal(operation)?;
        self.validate_dependencies(operation)?;
        self.occurrence_context_ids(operation)?;
        if let Some(edit) = edit {
            self.validate_composer_edit(owner, edit, true)?;
        }
        if consumption == ComposerConsumption::PreserveCurrent {
            return preflight(self);
        }
        if !ordered_references(&self.draft_companions(owner), captured)
            || self.composer_edit(owner) != edit
        {
            return Err(JournalError::Invalid(
                "current composer differs from the exact captured source",
            ));
        }
        let previous_companions = self.draft_companions.clone();
        let previous_edits = self.composer_edits.clone();
        self.draft_companions
            .retain(|reference| !captured.contains(reference));
        for record in &mut self.composer_edits {
            if record
                .owner
                .clone()
                .into_key()
                .as_ref()
                .is_ok_and(|key| key == owner)
                && record.current.as_ref() == edit
            {
                record.current = None;
            }
        }
        if let Err(error) = preflight(self) {
            self.draft_companions = previous_companions;
            self.composer_edits = previous_edits;
            return Err(error);
        }
        Ok(())
    }

    fn validate_composer_edit(
        &self,
        owner: &DraftKey,
        edit: &ComposerEditRef,
        complete: bool,
    ) -> Result<(), JournalError> {
        if edit.generation == 0
            || edit.revision == 0
            || edit.generation == u64::MAX
            || edit.revision == u64::MAX
            || edit.companions.is_empty()
            || edit.companions.len() > MAX_ENTRIES
            || edit.operation != edit.companions[0].operation
        {
            return Err(JournalError::Invalid("invalid compound edit identity"));
        }
        let operation = self
            .operation(&edit.operation)
            .ok_or(JournalError::Invalid("compound edit lost its take owner"))?;
        if operation.owner.draft_key()?.ne(owner) {
            return Err(JournalError::Invalid(
                "compound edit belongs to another draft",
            ));
        }
        let object = self.require_object(
            operation,
            &edit.object,
            QueueObjectKind::NativeCompanion,
            complete,
        )?;
        if object.generation != edit.generation
            || object.reference.total_bytes() != edit.byte_len
            || object.reference.sha256() != &edit.sha256
        {
            return Err(JournalError::Invalid(
                "compound edit differs from its immutable bytes",
            ));
        }
        let mut unique = HashSet::new();
        for reference in &edit.companions {
            let original = self
                .operation(&reference.operation)
                .ok_or(JournalError::Invalid("compound edit lost a source"))?;
            if !unique.insert(&reference.operation)
                || original.owner.draft_key()?.ne(owner)
                || !matches!(self.local_take(&reference.operation), Some(QueueLocalTake {
                    destination: QueueDestination::Draft { companion }, .. }) if companion == &reference.companion)
            {
                return Err(JournalError::Invalid(
                    "compound edit changed its original source owner",
                ));
            }
        }
        Ok(())
    }

    /// Admit the complete immutable write intent before the worker may put chunks.
    pub(crate) fn plan_composer_edit(
        &mut self,
        owner: &DraftKey,
        edit: ComposerEditRef,
        object: QueueObject,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        if object.id != edit.object
            || object.operation != edit.operation
            || self.object(&object.id).is_some()
            || self.objects.len() >= MAX_ENTRIES
            || !ordered_references(&self.draft_companions(owner), &edit.companions)
        {
            return Err(JournalError::Invalid(
                "compound edit has no current draft authority",
            ));
        }
        let previous = self.composer_edits.clone();
        self.objects.push(object);
        let index = match self.composer_edits.iter().position(|record| {
            record
                .owner
                .clone()
                .into_key()
                .as_ref()
                .is_ok_and(|key| key == owner)
        }) {
            Some(index) => index,
            None => {
                self.composer_edits.push(ComposerEditRecord {
                    owner: Owner::capture(owner),
                    current: None,
                    versions: Vec::new(),
                });
                self.composer_edits.len() - 1
            }
        };
        self.composer_edits[index].versions.push(edit);
        if let Err(error) = preflight(self) {
            self.objects.pop();
            self.composer_edits = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Previous versions remain rooted until this publication is durably saved.
    pub(crate) fn publish_composer_edit(
        &mut self,
        owner: &DraftKey,
        edit: &ComposerEditRef,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        self.validate_composer_edit(owner, edit, true)?;
        if !ordered_references(&self.draft_companions(owner), &edit.companions) {
            return Err(JournalError::Invalid("compound edit draft was cleared"));
        }
        let index = self
            .composer_edits
            .iter()
            .position(|record| {
                record
                    .owner
                    .clone()
                    .into_key()
                    .as_ref()
                    .is_ok_and(|key| key == owner)
                    && record.versions.contains(edit)
            })
            .ok_or(JournalError::Invalid("compound edit has no prepared owner"))?;
        if self.composer_edits[index]
            .current
            .as_ref()
            .is_some_and(|current| {
                (current.generation, current.revision) > (edit.generation, edit.revision)
            })
        {
            return Err(JournalError::Invalid("obsolete compound edit publication"));
        }
        let previous = self.composer_edits[index].current.replace(edit.clone());
        if let Err(error) = preflight(self) {
            self.composer_edits[index].current = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Explicit draft discard. Unresolved takes and consuming operations remain.
    pub(crate) fn clear_composer_document(
        &mut self,
        owner: &DraftKey,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let old_companions = self.draft_companions.clone();
        let old_edits = self.composer_edits.clone();
        let old_cleared = self.cleared_takes.clone();
        let operations: Vec<_> = self
            .operations
            .iter()
            .filter(|operation| {
                operation
                    .owner
                    .draft_key()
                    .as_ref()
                    .is_ok_and(|key| key == owner)
                    && matches!(
                        operation.action,
                        QueueAction::Take {
                            purpose: QueueTakePurpose::Edit,
                            ..
                        }
                    )
            })
            .map(|operation| operation.id.clone())
            .collect();
        self.draft_companions
            .retain(|reference| !operations.contains(&reference.operation));
        for id in operations {
            if !self.cleared_takes.contains(&id) {
                self.cleared_takes.push(id);
            }
        }
        for record in &mut self.composer_edits {
            if record
                .owner
                .clone()
                .into_key()
                .as_ref()
                .is_ok_and(|key| key == owner)
            {
                record.current = None;
            }
        }
        if let Err(error) = preflight(self) {
            self.draft_companions = old_companions;
            self.composer_edits = old_edits;
            self.cleared_takes = old_cleared;
            return Err(error);
        }
        Ok(())
    }

    /// Call only after the current edit/clear reference switch committed. Creates
    /// ordinary durable deletion intents; it never deletes bytes itself.
    pub(crate) fn retire_composer_edits(
        &mut self,
        owner: &DraftKey,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let mut candidate = self.clone();
        let mut pinned: HashSet<_> = candidate
            .operations
            .iter()
            .filter_map(QueueOperation::composer_edit)
            .map(|edit| edit.object.clone())
            .collect();
        pinned.extend(
            candidate
                .captured_sources
                .iter()
                .filter_map(Weak::upgrade)
                .filter_map(|source| source.edit.as_ref().map(|edit| edit.object.clone())),
        );
        let mut retiring = Vec::new();
        for record in &mut candidate.composer_edits {
            if !record
                .owner
                .clone()
                .into_key()
                .as_ref()
                .is_ok_and(|key| key == owner)
            {
                continue;
            }
            let current = record.current.clone();
            record.versions.retain(|version| {
                let keep = current.as_ref() == Some(version)
                    || pinned.contains(&version.object)
                    || current.as_ref().is_some_and(|current| {
                        (version.generation, version.revision)
                            > (current.generation, current.revision)
                    });
                if !keep {
                    retiring.push(version.object.clone());
                }
                keep
            });
        }
        for id in &retiring {
            let object = candidate.object(id).ok_or(JournalError::Invalid(
                "retired compound edit lost its object",
            ))?;
            let chunks = object
                .reference
                .chunks()
                .iter()
                .map(|chunk| {
                    chunk
                        .descriptor()
                        .map(|descriptor| descriptor.id().as_str().to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            candidate.retired.extend(chunks);
        }
        candidate
            .objects
            .retain(|object| !retiring.contains(&object.id));
        candidate
            .composer_edits
            .retain(|record| !record.versions.is_empty());
        preflight(&candidate)?;
        *self = candidate;
        Ok(())
    }
    pub(crate) fn retain_take_native_source(
        &mut self,
        id: &QueueOperationId,
        source: &QueueNativeSource,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let index = self
            .operations
            .iter()
            .position(|operation| &operation.id == id)
            .ok_or(JournalError::Invalid(
                "missing native take destination owner",
            ))?;
        if self.operations[index].owner.draft_key()? != source.owner
            || !matches!(
                self.operations[index].action,
                QueueAction::Take {
                    phase: QueueTakePhase::Transferring(_),
                    ..
                }
            )
        {
            return Err(JournalError::Invalid(
                "native destination belongs to another take",
            ));
        }
        let old_native = self.operations[index].native_roots.clone();
        let old_prepared = self.operations[index].prepared_roots.clone();
        for root in native_roots(source) {
            if !self.operations[index].native_roots.contains(&root) {
                self.operations[index].native_roots.push(root);
            }
        }
        for root in prepared_roots(source) {
            if !self.operations[index].prepared_roots.contains(&root) {
                self.operations[index].prepared_roots.push(root);
            }
        }
        if let Err(error) = preflight(self) {
            self.operations[index].native_roots = old_native;
            self.operations[index].prepared_roots = old_prepared;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn draft_companions(&self, owner: &DraftKey) -> Vec<DraftCompanionRef> {
        self.draft_companions
            .iter()
            .filter(|reference| {
                self.operation(&reference.operation)
                    .is_some_and(|operation| {
                        operation
                            .owner
                            .draft_key()
                            .as_ref()
                            .is_ok_and(|key| key == owner)
                    })
            })
            .cloned()
            .collect()
    }

    /// Called in the ordinary send's atomic capture transition. Newer appends
    /// remain with the composer. A failed snapshot admission consumes nothing.
    #[cfg(test)]
    pub(crate) fn consume_draft_companions(
        &mut self,
        owner: &DraftKey,
        captured: &[DraftCompanionRef],
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let owned = self.draft_companions(owner);
        if !ordered_references(&owned, captured) {
            return Err(JournalError::Invalid(
                "composer companion capture lost ownership",
            ));
        }
        if !captured.is_empty()
            && !self.operations.iter().any(|operation| {
                operation
                    .owner
                    .draft_key()
                    .as_ref()
                    .is_ok_and(|key| key == owner)
                    && operation.dependencies() == captured
                    && matches!(operation.source(), Some(QueueSource::Companion(_)))
            })
        {
            return Err(JournalError::Invalid(
                "composer companions have no published consuming owner",
            ));
        }
        let previous = self.draft_companions.clone();
        self.draft_companions
            .retain(|reference| !captured.contains(reference));
        if let Err(error) = preflight(self) {
            self.draft_companions = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn record_take_send_state(
        &mut self,
        id: &QueueOperationId,
        state: QueueTakeSendState,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing queued send owner"))?;
        let QueueAction::Take {
            purpose,
            request,
            phase: QueueTakePhase::Acknowledged { local, receipt },
        } = &operation.action
        else {
            return Err(JournalError::Invalid(
                "queued send has no acknowledged local capture",
            ));
        };
        if !matches!(purpose, QueueTakePurpose::Send { .. }) {
            return Err(JournalError::Invalid(
                "an Edit has no queued send admission",
            ));
        }
        if local.send == QueueTakeSendState::Admitted && state != QueueTakeSendState::Admitted {
            return Err(JournalError::Invalid(
                "admitted queued send cannot be reopened",
            ));
        }
        let mut local = local.clone();
        local.send = state;
        let action = QueueAction::Take {
            purpose: purpose.clone(),
            request: request.clone(),
            phase: QueueTakePhase::Acknowledged {
                local,
                receipt: receipt.clone(),
            },
        };
        self.transition(id, action, preflight)
    }

    /// A transfer's original sealed index is immutable. Only its one-time local
    /// byte bindings may be filled after materialization.
    pub(crate) fn bind_take_files(
        &mut self,
        id: &QueueOperationId,
        files: Vec<Vec<QueueFileBinding>>,
        attachments: &AttachmentCatalog,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing take file owner"))?;
        let QueueAction::Take {
            purpose,
            request,
            phase: QueueTakePhase::Transferring(transfer),
        } = &operation.action
        else {
            return Err(JournalError::Invalid(
                "take has no transferable file capture",
            ));
        };
        if transfer.items.len() != files.len()
            || transfer.items.iter().any(|item| !item.files.is_empty())
        {
            return Err(JournalError::Invalid(
                "take file bindings are already fixed",
            ));
        }
        let mut transfer = transfer.clone();
        for (item, files) in transfer.items.iter_mut().zip(files) {
            item.files = files;
        }
        self.validate_transfer(operation, &transfer, attachments, false)?;
        let action = QueueAction::Take {
            purpose: purpose.clone(),
            request: request.clone(),
            phase: QueueTakePhase::Transferring(transfer),
        };
        self.transition(id, action, preflight)
    }

    pub(crate) fn operations(&self) -> &[QueueOperation] {
        &self.operations
    }
    pub(crate) fn objects(&self) -> &[QueueObject] {
        &self.objects
    }
    pub(crate) fn operation(&self, id: &QueueOperationId) -> Option<&QueueOperation> {
        self.operations.iter().find(|operation| &operation.id == id)
    }
    pub(crate) fn object(&self, id: &QueueObjectId) -> Option<&QueueObject> {
        self.objects.iter().find(|object| &object.id == id)
    }

    pub(crate) fn prepare_enqueue(
        &mut self,
        id: &QueueOperationId,
        payload: QueueObjectId,
        request: QueueJson,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing enqueue owner"))?;
        let QueueAction::Enqueue {
            source: QueueSource::Companion(companion),
            phase: QueueEnqueuePhase::Capturing,
        } = &operation.action
        else {
            return Err(JournalError::Invalid(
                "enqueue source has not been published",
            ));
        };
        self.validate_enqueue_request(operation, &payload, &request)?;
        self.require_composer_inputs(operation)?;
        let action = QueueAction::Enqueue {
            source: QueueSource::Companion(companion.clone()),
            phase: QueueEnqueuePhase::Ready { payload, request },
        };
        self.transition(id, action, preflight)
    }

    pub(crate) fn record_enqueue_accepted(
        &mut self,
        id: &QueueOperationId,
        receipt: QueueJson,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing enqueue receipt owner"))?;
        let QueueAction::Enqueue { source, phase } = &operation.action else {
            return Err(JournalError::Invalid("enqueue has no retained request"));
        };
        let (payload, request, previous) = match phase {
            QueueEnqueuePhase::Ready { payload, request }
            | QueueEnqueuePhase::Unknown { payload, request } => (payload, request, None),
            QueueEnqueuePhase::Accepted {
                payload,
                request,
                receipt,
            }
            | QueueEnqueuePhase::RetryPrepared {
                payload,
                request,
                receipt,
                ..
            } => (payload, request, Some(receipt)),
            _ => return Err(JournalError::Invalid("enqueue has no retained request")),
        };
        validate_receipt(&receipt, operation, "enqueue")?;
        self.validate_enqueue_preparation_receipt(operation, &receipt)?;
        if let Some(previous) = previous {
            let previous = previous.value()?;
            let observed = receipt.value()?;
            if previous.get("receipt") != observed.get("receipt")
                || previous
                    .get("attempt")
                    .and_then(serde_json::Value::as_u64)
                    .zip(observed.get("attempt").and_then(serde_json::Value::as_u64))
                    .is_some_and(|(old, new)| new < old)
            {
                return Err(JournalError::Invalid(
                    "enqueue observation changed its immutable receipt or attempt order",
                ));
            }
        }
        if let QueueEnqueuePhase::RetryPrepared { retry, .. } = phase {
            let observed = receipt.value()?;
            let pending = retry
                .value()?
                .get("attempt")
                .and_then(serde_json::Value::as_u64);
            if observed.get("attempt").and_then(serde_json::Value::as_u64) < pending
                && !matches!(
                    observed.get("state").and_then(serde_json::Value::as_str),
                    Some("dispatched" | "removed" | "taken" | "deleted")
                )
            {
                return Err(JournalError::Invalid(
                    "enqueue observation has not resolved its saved retry authorization",
                ));
            }
        }
        let action = QueueAction::Enqueue {
            source: source.clone(),
            phase: QueueEnqueuePhase::Accepted {
                payload: payload.clone(),
                request: request.clone(),
                receipt,
            },
        };
        self.transition(id, action, preflight)
    }

    pub(crate) fn prepare_enqueue_retry(
        &mut self,
        id: &QueueOperationId,
        retry: QueueJson,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing enqueue retry owner"))?;
        let QueueAction::Enqueue {
            source,
            phase:
                QueueEnqueuePhase::Accepted {
                    payload,
                    request,
                    receipt,
                },
        } = &operation.action
        else {
            return Err(JournalError::Invalid(
                "enqueue retry has no accepted receipt",
            ));
        };
        validate_enqueue_retry(&retry, receipt, &operation.owner)?;
        let action = QueueAction::Enqueue {
            source: source.clone(),
            phase: QueueEnqueuePhase::RetryPrepared {
                payload: payload.clone(),
                request: request.clone(),
                receipt: receipt.clone(),
                retry,
            },
        };
        self.transition(id, action, preflight)
    }

    pub(crate) fn record_take_acknowledged(
        &mut self,
        permit: &QueueAcknowledgementPermit,
        receipt: QueueJson,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let id = permit.operation();
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing acknowledgement owner"))?;
        let QueueAction::Take {
            purpose,
            request,
            phase: QueueTakePhase::LocalOwned(local) | QueueTakePhase::AcknowledgementUnknown(local),
        } = &operation.action
        else {
            return Err(JournalError::Invalid(
                "take has no committed local destination",
            ));
        };
        if permit.request() != &local.acknowledge || permit.receipt() != &local.transfer.receipt {
            return Err(JournalError::Invalid(
                "acknowledgement permit belongs to another destination",
            ));
        }
        validate_receipt(&receipt, operation, "take")?;
        let accepted = receipt.value()?;
        let retained = local.transfer.receipt.value()?;
        if accepted.get("state").and_then(serde_json::Value::as_str) != Some("acknowledged")
            || accepted.get("receipt") != retained.get("receipt")
        {
            return Err(JournalError::Invalid(
                "acknowledgement changed its retained transfer receipt",
            ));
        }
        let action = QueueAction::Take {
            purpose: purpose.clone(),
            request: request.clone(),
            phase: QueueTakePhase::Acknowledged {
                local: local.clone(),
                receipt,
            },
        };
        self.transition(id, action, preflight)
    }

    pub(crate) fn prepare_prefix(
        &mut self,
        id: &QueueOperationId,
        handoff: QueuePrefixHandoff,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing prefix owner"))?;
        let QueueAction::Prefix {
            message,
            source: QueueSource::Companion(companion),
            request,
            phase: QueuePrefixPhase::Capturing,
        } = &operation.action
        else {
            return Err(JournalError::Invalid(
                "prefix composer has not been published",
            ));
        };
        self.require_composer_inputs(operation)?;
        validate_real_prefix_request(request, &operation.owner, message)?;
        self.validate_prefix(operation, message, request, &handoff)?;
        let action = QueueAction::Prefix {
            message: message.clone(),
            source: QueueSource::Companion(companion.clone()),
            request: request.clone(),
            phase: QueuePrefixPhase::Previewed(handoff),
        };
        self.transition(id, action, preflight)
    }

    /// The transport validates the answer first. Revision changes alone are harmless.
    pub(crate) fn record_prefix_reserved(
        &mut self,
        id: &QueueOperationId,
        receipt: QueueJson,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing prefix reservation owner"))?;
        let QueueAction::Prefix {
            message,
            source,
            request,
            phase:
                QueuePrefixPhase::Previewed(handoff) | QueuePrefixPhase::ReservationUnknown(handoff),
        } = &operation.action
        else {
            return Err(JournalError::Invalid("prefix lacks its frozen preview"));
        };
        let expected: PrefixReceipt = serde_json::from_slice(handoff.receipt.bytes())
            .map_err(|_| JournalError::Invalid("invalid frozen prefix receipt"))?;
        let actual: PrefixReceipt = serde_json::from_slice(receipt.bytes())
            .map_err(|_| JournalError::Invalid("invalid prefix reservation receipt"))?;
        if expected != actual {
            return Err(JournalError::Invalid(
                "reservation differs from frozen prefix preview",
            ));
        }
        let mut retained = handoff.clone();
        retained.receipt = receipt;
        let action = QueueAction::Prefix {
            message: message.clone(),
            source: source.clone(),
            request: request.clone(),
            phase: QueuePrefixPhase::Reserved(retained),
        };
        self.transition(id, action, preflight)
    }

    /// Called only for authoritative admission of this exact saved message identity.
    pub(crate) fn record_prefix_admitted(
        &mut self,
        id: &QueueOperationId,
        admitted: &MessageId,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing admitted prefix owner"))?;
        let QueueAction::Prefix {
            message,
            source,
            request,
            phase:
                QueuePrefixPhase::Reserved(handoff)
                | QueuePrefixPhase::ReservationUnknown(handoff)
                | QueuePrefixPhase::Previewed(handoff)
                | QueuePrefixPhase::Admitted(handoff),
        } = &operation.action
        else {
            return Err(JournalError::Invalid(
                "prefix has no frozen admission capture",
            ));
        };
        if admitted.as_str() != message {
            return Err(JournalError::Invalid(
                "prefix admission changed its message identity",
            ));
        }
        let action = QueueAction::Prefix {
            message: message.clone(),
            source: source.clone(),
            request: request.clone(),
            phase: QueuePrefixPhase::Admitted(handoff.clone()),
        };
        self.transition(id, action, preflight)
    }

    /// Replaces only the internal capture intent, after its complete source is sealed.
    /// An already prepared real request is immutable, including its upload labels.
    pub(crate) fn prepare_prefix_request(
        &mut self,
        id: &QueueOperationId,
        request: QueueJson,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing prefix request owner"))?;
        let QueueAction::Prefix {
            message,
            source: QueueSource::Companion(source),
            request: previous,
            phase: QueuePrefixPhase::Capturing,
        } = &operation.action
        else {
            return Err(JournalError::Invalid(
                "prefix request requires a published capturing source",
            ));
        };
        self.validate_source_seal(operation)?;
        self.require_composer_inputs(operation)?;
        let body = validate_real_prefix_request(&request, &operation.owner, message)?;
        let Some(QueueSourceIntent::Prefix(intent)) = &operation.source_intent else {
            return Err(JournalError::Invalid(
                "prefix has no original compound capture intent",
            ));
        };
        if body.item_ids != intent.item_ids || body.delivery != intent.delivery {
            return Err(JournalError::Invalid(
                "prefix request changed its original items or delivery",
            ));
        }
        if previous.value()?.get("kind").is_none() && previous != &request {
            return Err(JournalError::Invalid(
                "prepared prefix request is immutable",
            ));
        }
        let pointer = body.pointer()?;
        if !self.objects.iter().any(|object| {
            object.operation == operation.id
                && object.kind == QueueObjectKind::Payload
                && object.verified()
                && object.reference.total_bytes() == pointer.total_bytes()
                && object.reference.sha256() == &pointer.sha256()
        }) {
            return Err(JournalError::Invalid(
                "prefix composer pointer has no verified local payload",
            ));
        }
        let action = QueueAction::Prefix {
            message: message.clone(),
            source: QueueSource::Companion(source.clone()),
            request,
            phase: QueuePrefixPhase::Capturing,
        };
        self.transition(id, action, preflight)
    }

    pub(crate) fn prepare_local_send(
        &mut self,
        id: &QueueOperationId,
        frame: QueueObjectId,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing local send owner"))?;
        let QueueAction::Send {
            message,
            source: QueueSource::Companion(source),
            delivery,
            context_ids,
            phase: QueueSendPhase::Capturing,
        } = &operation.action
        else {
            return Err(JournalError::Invalid(
                "local send requires its published original source",
            ));
        };
        self.validate_source_seal(operation)?;
        self.require_composer_inputs(operation)?;
        self.require_object(operation, &frame, QueueObjectKind::FrozenSend, true)?;
        if context_ids != &self.source_context_ids(operation)? {
            return Err(JournalError::Invalid(
                "local send changed its original context identities",
            ));
        }
        let action = QueueAction::Send {
            message: message.clone(),
            source: QueueSource::Companion(source.clone()),
            delivery: *delivery,
            context_ids: context_ids.clone(),
            phase: QueueSendPhase::Prepared { frame },
        };
        self.transition(id, action, preflight)
    }

    pub(crate) fn mark_send_unknown(
        &mut self,
        id: &QueueOperationId,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing local send dispatch owner"))?;
        let QueueAction::Send {
            message,
            source,
            delivery,
            context_ids,
            phase: QueueSendPhase::Prepared { frame } | QueueSendPhase::Unknown { frame },
        } = &operation.action
        else {
            return Err(JournalError::Invalid(
                "local send has no prepared outgoing frame",
            ));
        };
        let action = QueueAction::Send {
            message: message.clone(),
            source: source.clone(),
            delivery: *delivery,
            context_ids: context_ids.clone(),
            phase: QueueSendPhase::Unknown {
                frame: frame.clone(),
            },
        };
        self.transition(id, action, preflight)
    }

    /// Only confirmed deletion of these captured runtime/session identities may
    /// settle their local sends. Original source, configuration and claims stay sealed.
    pub(crate) fn delete_local_sends(
        &mut self,
        runtime: &RuntimeId,
        sessions: &[SessionId],
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<Vec<QueueOperationId>, JournalError> {
        let mut candidate = self.clone();
        let mut deleted = Vec::new();
        for operation in &mut candidate.operations {
            if operation.owner.runtime() != runtime.as_str()
                || !sessions
                    .iter()
                    .any(|session| session.as_str() == operation.owner.session())
            {
                continue;
            }
            if let QueueAction::Send { phase, .. } = &mut operation.action {
                *phase = QueueSendPhase::Deleted;
                deleted.push(operation.id.clone());
            }
        }
        for job in &mut candidate.local_send_retirements {
            if job.owner.runtime() == runtime.as_str()
                && sessions
                    .iter()
                    .any(|session| session.as_str() == job.owner.session())
            {
                job.deleted = true;
                deleted.push(job.operation.clone());
            }
        }
        let mut owners = candidate
            .operations
            .iter()
            .filter(|operation| {
                operation.owner.runtime() == runtime.as_str()
                    && sessions
                        .iter()
                        .any(|session| session.as_str() == operation.owner.session())
            })
            .map(|operation| operation.owner.draft_key())
            .collect::<Result<Vec<_>, _>>()?;
        for record in &candidate.composer_edits {
            let owner = record.owner.clone().into_key()?;
            if owner.runtime() == runtime
                && matches!(owner.target(), DraftTarget::Session(session) if sessions.contains(session))
                && !owners.contains(&owner)
            {
                owners.push(owner);
            }
        }
        for owner in owners {
            candidate.clear_composer_document(&owner, |_| Ok(()))?;
        }
        deleted.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        deleted.dedup();
        preflight(&candidate)?;
        *self = candidate;
        Ok(deleted)
    }

    /// The caller supplies an authoritative observation of the saved prompt ID.
    pub(crate) fn record_send_admitted(
        &mut self,
        id: &QueueOperationId,
        admitted: &MessageId,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing local send admission owner"))?;
        let QueueAction::Send {
            message,
            source,
            delivery,
            context_ids,
            phase:
                QueueSendPhase::Prepared { frame }
                | QueueSendPhase::Unknown { frame }
                | QueueSendPhase::Admitted { frame },
        } = &operation.action
        else {
            return Err(JournalError::Invalid(
                "local send has no frozen admission capture",
            ));
        };
        if admitted.as_str() != message {
            return Err(JournalError::Invalid(
                "local send admission changed its message identity",
            ));
        }
        let action = QueueAction::Send {
            message: message.clone(),
            source: source.clone(),
            delivery: *delivery,
            context_ids: context_ids.clone(),
            phase: QueueSendPhase::Admitted {
                frame: frame.clone(),
            },
        };
        self.transition(id, action, preflight)
    }

    fn transition(
        &mut self,
        id: &QueueOperationId,
        action: QueueAction,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let index = self
            .operations
            .iter()
            .position(|operation| &operation.id == id)
            .ok_or(JournalError::Invalid("missing queue transition owner"))?;
        let previous = std::mem::replace(&mut self.operations[index].action, action);
        if let Err(error) = self
            .validate_context_reservations()
            .and_then(|()| preflight(self))
        {
            self.operations[index].action = previous;
            return Err(error);
        }
        Ok(())
    }

    fn validate_enqueue_request(
        &self,
        operation: &QueueOperation,
        payload: &QueueObjectId,
        request: &QueueJson,
    ) -> Result<(), JournalError> {
        validate_directory_body(request, &operation.owner)?;
        let body = request.value()?;
        let pointer = body
            .get("item")
            .ok_or(JournalError::Invalid("enqueue has no payload pointer"))?;
        let pointer = QueuePayloadPointer::from_body(
            &serde_json::to_vec(pointer)
                .map_err(|_| JournalError::Invalid("queue pointer cannot be encoded"))?,
        )
        .map_err(|_| JournalError::Invalid("invalid retained enqueue pointer"))?;
        let object = self.require_object(operation, payload, QueueObjectKind::Payload, true)?;
        if object.reference.total_bytes() != pointer.total_bytes()
            || object.reference.sha256() != &pointer.sha256()
        {
            return Err(JournalError::Invalid(
                "enqueue pointer differs from owned payload bytes",
            ));
        }
        Ok(())
    }

    /// Ordinary first-use callers share the same atomic claim transition. Repeats,
    /// including context-free repeats, require explicit occurrence admission.
    pub(crate) fn admit_operation(
        &mut self,
        operation: QueueOperation,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), (JournalError, Box<QueueOperation>)> {
        if operation.dependencies().iter().any(|reference| {
            self.operation(&reference.operation).is_some_and(|take| {
                matches!(
                    take.take_source_use,
                    Some(QueueTakeSourceUse::Claimed { .. })
                )
            })
        }) {
            return Err((
                JournalError::Invalid("repeat companion requires explicit occurrence admission"),
                Box::new(operation),
            ));
        }
        self.admit_composer_operation(operation, Vec::new(), preflight)
    }

    /// Count replacements from authoritative original indexes. Allocate these IDs
    /// and admit in the same synchronous owner update; admission checks again.
    pub(crate) fn required_companion_rebinding_ids(
        &self,
        operation: &QueueOperation,
    ) -> Result<usize, JournalError> {
        if operation.source_occurrences.is_some() {
            return Err(JournalError::Invalid(
                "an existing occurrence cannot acquire new context identities",
            ));
        }
        let originals = self.original_source_occurrences(operation)?;
        let mut required = 0usize;
        for (source, ids) in originals {
            match self
                .operation(&source.take)
                .and_then(QueueOperation::take_source_use)
            {
                Some(QueueTakeSourceUse::Unused) => {}
                Some(QueueTakeSourceUse::Claimed { .. }) => {
                    required = required
                        .checked_add(ids.len())
                        .filter(|count| *count <= MAX_ENTRIES)
                        .ok_or(JournalError::Invalid(
                            "too many companion replacement identities",
                        ))?;
                }
                None => {
                    return Err(JournalError::Invalid(
                        "original Take source-use history is unknown",
                    ));
                }
            }
        }
        Ok(required)
    }

    /// Admit independent read-only occurrences without changing current composer
    /// refs or original Take/index/proof bytes. First claims and mappings commit
    /// together before global identity reservation. Refusal restores both owners.
    pub(crate) fn admit_composer_operation(
        &mut self,
        operation: QueueOperation,
        fresh_context_ids: Vec<String>,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), (JournalError, Box<QueueOperation>)> {
        self.admit_composer_operation_with_capture(operation, fresh_context_ids, None, preflight)
    }

    /// The edit plan, consumer and current-ledger claims become one candidate.
    pub(crate) fn admit_composer_operation_from_capture(
        &mut self,
        operation: QueueOperation,
        capture: &CapturedComposerSource,
        pending: Option<QueueObject>,
        fresh_context_ids: Vec<String>,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        if capture.consumer != operation.id
            || capture.owner != operation.owner.draft_key()?
            || capture.companions.as_slice() != operation.dependencies()
            || capture
                .edit
                .as_ref()
                .is_some_and(|edit| operation.composer_edit() != Some(edit))
        {
            return Err(JournalError::Invalid(
                "submission changed its immutable registered capture",
            ));
        }
        let mut candidate = self.clone();
        if let Some(object) = pending {
            let edit = operation
                .composer_edit()
                .ok_or(JournalError::Invalid(
                    "pending edit has no consuming source reference",
                ))?
                .clone();
            if object.id != edit.object
                || object.operation != edit.operation
                || object.generation != edit.generation
                || object.reference.total_bytes() != edit.byte_len
                || object.reference.sha256() != &edit.sha256
                || candidate.objects.len() >= MAX_ENTRIES
                || candidate.object(&object.id).is_some()
            {
                return Err(JournalError::Invalid("invalid consuming edit plan"));
            }
            candidate.objects.push(object);
            let index = match candidate.composer_edits.iter().position(|record| {
                record
                    .owner
                    .clone()
                    .into_key()
                    .as_ref()
                    .is_ok_and(|owner| owner == &capture.owner)
            }) {
                Some(index) => index,
                None => {
                    candidate.composer_edits.push(ComposerEditRecord {
                        owner: Owner::capture(&capture.owner),
                        current: None,
                        versions: Vec::new(),
                    });
                    candidate.composer_edits.len() - 1
                }
            };
            candidate.composer_edits[index].versions.push(edit);
        }
        candidate
            .admit_composer_operation_with_capture(
                operation,
                fresh_context_ids,
                Some(capture),
                preflight,
            )
            .map_err(|(error, _)| error)?;
        *self = candidate;
        Ok(())
    }

    fn admit_composer_operation_with_capture(
        &mut self,
        mut operation: QueueOperation,
        fresh_context_ids: Vec<String>,
        captured: Option<&CapturedComposerSource>,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), (JournalError, Box<QueueOperation>)> {
        if self.operations.len() >= MAX_ENTRIES
            || self.operation(&operation.id).is_some()
            || self.local_send_retirement(&operation.id).is_some()
            || self.operations.iter().any(|take| {
                matches!(&take.take_source_use,
                Some(QueueTakeSourceUse::Claimed { consumer }) if consumer == &operation.id)
            })
        {
            return Err((
                JournalError::Invalid("duplicate or excessive queue operations"),
                Box::new(operation),
            ));
        }
        let preparation = (|| {
            if operation.source_occurrences.is_some() {
                return Err(JournalError::Invalid(
                    "an existing occurrence cannot acquire new context identities",
                ));
            }
            validate_context_ids(&fresh_context_ids)?;
            let owned = self.draft_companions(&operation.owner.draft_key()?);
            if !ordered_references(&owned, operation.dependencies())
                && !captured.is_some_and(|capture| {
                    capture.consumer == operation.id
                        && operation
                            .owner
                            .draft_key()
                            .as_ref()
                            .is_ok_and(|owner| owner == &capture.owner)
                        && capture.companions.as_slice() == operation.dependencies()
                })
            {
                return Err(JournalError::Invalid(
                    "captured companion dependencies changed their original order",
                ));
            }
            if !operation.dependencies().is_empty() {
                if !capturing_composer(&operation) {
                    return Err(JournalError::Invalid(
                        "new source occurrences require a capturing consumer",
                    ));
                }
                self.validate_source_seal(&operation)?;
            }
            let mut sources = self.original_source_occurrences(&operation)?;
            let mut next = 0usize;
            for (source, ids) in &mut sources {
                match self
                    .operation(&source.take)
                    .and_then(QueueOperation::take_source_use)
                {
                    Some(QueueTakeSourceUse::Unused) => {}
                    Some(QueueTakeSourceUse::Claimed { .. }) => {
                        let end = next
                            .checked_add(ids.len())
                            .filter(|end| *end <= fresh_context_ids.len())
                            .ok_or(JournalError::Invalid(
                                "companion replacement identity count changed",
                            ))?;
                        source.context_rebinding = Some(fresh_context_ids[next..end].to_vec());
                        next = end;
                    }
                    None => {
                        return Err(JournalError::Invalid(
                            "original Take source-use history is unknown",
                        ));
                    }
                }
            }
            if next != fresh_context_ids.len() {
                return Err(JournalError::Invalid(
                    "companion replacement identity count changed",
                ));
            }
            let claims = operation
                .dependencies()
                .iter()
                .filter_map(|reference| {
                    self.operations.iter().position(|take| {
                        take.id == reference.operation
                            && matches!(take.take_source_use, Some(QueueTakeSourceUse::Unused))
                    })
                })
                .collect::<Vec<_>>();
            Ok((
                sources
                    .into_iter()
                    .map(|(source, _)| source)
                    .collect::<Vec<_>>(),
                claims,
            ))
        })();
        let (sources, claims) = match preparation {
            Ok(preparation) => preparation,
            Err(error) => return Err((error, Box::new(operation))),
        };
        if !sources.is_empty() {
            operation.source_occurrences = Some(sources);
        }
        for index in &claims {
            self.operations[*index].take_source_use = Some(QueueTakeSourceUse::Claimed {
                consumer: operation.id.clone(),
            });
        }
        let previous_contexts = if let QueueAction::Send { context_ids, .. } = &operation.action {
            Some(context_ids.clone())
        } else {
            None
        };
        let contexts = match self.source_context_ids(&operation) {
            Ok(contexts) => contexts,
            Err(error) => {
                for index in claims {
                    self.operations[index].take_source_use = Some(QueueTakeSourceUse::Unused);
                }
                operation.source_occurrences = None;
                return Err((error, Box::new(operation)));
            }
        };
        if let QueueAction::Send { context_ids, .. } = &mut operation.action {
            *context_ids = contexts;
        }
        self.operations.push(operation);
        if let Err(error) = self
            .validate_source_claims()
            .and_then(|()| self.validate_context_reservations())
            .and_then(|()| preflight(self))
        {
            let mut operation = self
                .operations
                .pop()
                .expect("the candidate was appended synchronously");
            for index in claims {
                self.operations[index].take_source_use = Some(QueueTakeSourceUse::Unused);
            }
            operation.source_occurrences = None;
            if let (Some(previous), QueueAction::Send { context_ids, .. }) =
                (previous_contexts, &mut operation.action)
            {
                *context_ids = previous;
            }
            return Err((error, Box::new(operation)));
        }
        Ok(())
    }

    pub(crate) fn admit_object(
        &mut self,
        object: QueueObject,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), (JournalError, Box<QueueObject>)> {
        if self.objects.len() >= MAX_ENTRIES
            || self.object(&object.id).is_some()
            || self.operation(&object.operation).is_none()
        {
            return Err((
                JournalError::Invalid("invalid queue object owner or identity"),
                Box::new(object),
            ));
        }
        let operation = self
            .operation(&object.operation)
            .expect("object owner was checked");
        if matches!(
            operation.action(),
            QueueAction::Send {
                phase: QueueSendPhase::Deleted,
                ..
            }
        ) {
            return Err((
                JournalError::Invalid("deleted local send grants no byte-write authority"),
                Box::new(object),
            ));
        }
        if let Some(preparation) = &operation.preparation {
            let original_source = object.kind == QueueObjectKind::NativeCompanion
                && operation.source_seal.as_ref().is_some_and(|seal| {
                    seal.byte_len == object.reference.total_bytes()
                        && &seal.sha256 == object.reference.sha256()
                });
            if !original_source
                && (preparation
                    .files
                    .iter()
                    .any(|file| file.state == QueueDerivedFileState::Proposed)
                    || (object.kind != QueueObjectKind::NativeCompanion
                        && preparation.inputs_object.is_none()))
            {
                return Err((
                    JournalError::Invalid(
                        "prepared object precedes durable derived catalog custody",
                    ),
                    Box::new(object),
                ));
            }
        }
        self.objects.push(object);
        if let Err(error) = preflight(self) {
            let object = self
                .objects
                .pop()
                .expect("the candidate was appended synchronously");
            return Err((error, Box::new(object)));
        }
        Ok(())
    }

    pub(crate) fn record_chunk(
        &mut self,
        hold: &QueueChunkHold,
        receipt: &BlobReceipt,
    ) -> Result<(), JournalError> {
        if self.operation(&hold.operation).is_some_and(|operation| {
            matches!(
                operation.action(),
                QueueAction::Send {
                    phase: QueueSendPhase::Deleted,
                    ..
                }
            )
        }) {
            return Err(JournalError::Invalid(
                "deleted local send cannot publish a late chunk",
            ));
        }
        let object = self
            .objects
            .iter_mut()
            .find(|object| object.id == hold.object)
            .ok_or(JournalError::Invalid("missing queue preparation"))?;
        if object.hold(hold.ordinal).as_ref() != Some(hold)
            || receipt.commit != SpoolCommit::Durable
            || receipt.descriptor != hold.chunk.descriptor()?
        {
            return Err(JournalError::Invalid(
                "queue put is not the exact durable preparation",
            ));
        }
        let QueueObjectProgress::Writing { durable_ordinals } = &mut object.progress else {
            return Err(JournalError::Invalid("queue object already published"));
        };
        durable_ordinals.push(hold.ordinal);
        durable_ordinals.sort_unstable();
        Ok(())
    }

    pub(crate) fn record_verified(
        &mut self,
        proof: &QueueVerifiedCapture,
    ) -> Result<(), JournalError> {
        let verification = proof.verification();
        if self
            .operation(&verification.operation)
            .is_some_and(|operation| {
                matches!(
                    operation.action(),
                    QueueAction::Send {
                        phase: QueueSendPhase::Deleted,
                        ..
                    }
                )
            })
        {
            return Err(JournalError::Invalid(
                "deleted local send cannot publish late verification",
            ));
        }
        let object = self
            .objects
            .iter_mut()
            .find(|object| object.id == verification.object)
            .ok_or(JournalError::Invalid("missing verified queue object"))?;
        if !object.ready_to_verify() || object.verification() != *verification {
            return Err(JournalError::Invalid(
                "queue verification differs from retained object",
            ));
        }
        object.progress = QueueObjectProgress::Verified;
        Ok(())
    }

    /// The worker's whole-stream proof must match the complete native source bytes.
    /// The caller saves this transition before consuming the original composer.
    pub(crate) fn publish_source(
        &mut self,
        id: &QueueOperationId,
        proof: &QueueVerifiedCapture,
    ) -> Result<(), JournalError> {
        let verification = proof.verification();
        let object = self
            .object(&verification.object)
            .ok_or(JournalError::Invalid("missing source companion"))?;
        if &object.operation != id
            || object.kind != QueueObjectKind::NativeCompanion
            || !object.ready_to_verify()
            || object.verification() != *verification
        {
            return Err(JournalError::Invalid(
                "source companion lacks exact verification",
            ));
        }
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing queue source owner"))?;
        if operation.source_intent.is_some()
            || !matches!(
                operation.action,
                QueueAction::Enqueue {
                    phase: QueueEnqueuePhase::Capturing,
                    ..
                } | QueueAction::Prefix {
                    phase: QueuePrefixPhase::Capturing,
                    ..
                }
            )
        {
            return Err(JournalError::Invalid(
                "compound capture requires typed source publication",
            ));
        }
        let Some(QueueSource::Inline(source)) = operation.source() else {
            return Err(JournalError::Invalid("queue source is already spooled"));
        };
        let bytes = serde_json::to_vec(source)
            .map_err(|_| JournalError::Invalid("native queue companion cannot be encoded"))?;
        if bytes.len() as u64 != verification.reference.total_bytes()
            || sha256(&bytes) != *verification.reference.sha256()
        {
            return Err(JournalError::Invalid(
                "native queue source differs from companion",
            ));
        }
        self.record_verified(proof)?;
        let operation = self
            .operations
            .iter_mut()
            .find(|operation| &operation.id == id)
            .expect("source owner was checked before publication");
        match &mut operation.action {
            QueueAction::Enqueue { source, .. } | QueueAction::Prefix { source, .. } => {
                *source = QueueSource::Companion(verification.object.clone());
            }
            QueueAction::Take { .. } | QueueAction::Send { .. } => {
                unreachable!("native-only source kind was checked")
            }
        }
        Ok(())
    }

    pub(crate) fn publish_prefix_source(
        &mut self,
        id: &QueueOperationId,
        proof: &QueueVerifiedCapture,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        if !matches!(
            self.operation(id).map(|operation| &operation.action),
            Some(QueueAction::Prefix {
                phase: QueuePrefixPhase::Capturing,
                ..
            })
        ) {
            return Err(JournalError::Invalid(
                "prefix source publication requires capturing ownership",
            ));
        }
        self.publish_compound_source(id, proof, preflight)
    }

    pub(crate) fn publish_send_source(
        &mut self,
        id: &QueueOperationId,
        proof: &QueueVerifiedCapture,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        if !matches!(
            self.operation(id).map(|operation| &operation.action),
            Some(QueueAction::Send {
                phase: QueueSendPhase::Capturing,
                ..
            })
        ) {
            return Err(JournalError::Invalid(
                "local send source publication requires capturing ownership",
            ));
        }
        self.publish_compound_source(id, proof, preflight)
    }

    pub(crate) fn publish_enqueue_source(
        &mut self,
        id: &QueueOperationId,
        proof: &QueueVerifiedCapture,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        if !matches!(
            self.operation(id).map(|operation| &operation.action),
            Some(QueueAction::Enqueue {
                phase: QueueEnqueuePhase::Capturing,
                ..
            })
        ) {
            return Err(JournalError::Invalid(
                "compound enqueue publication requires capturing ownership",
            ));
        }
        self.publish_compound_source(id, proof, preflight)
    }

    fn publish_compound_source(
        &mut self,
        id: &QueueOperationId,
        proof: &QueueVerifiedCapture,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing compound source owner"))?;
        if let Some(edit) = operation.composer_edit() {
            self.validate_composer_edit(&operation.owner.draft_key()?, edit, true)?;
        }
        let bytes = operation.compound_source_bytes()?;
        let seal = operation.source_seal.as_ref().ok_or(JournalError::Invalid(
            "compound source has no original seal",
        ))?;
        let verification = proof.verification();
        let object = self.require_object(
            operation,
            &verification.object,
            QueueObjectKind::NativeCompanion,
            false,
        )?;
        if !object.ready_to_verify()
            || object.verification() != *verification
            || bytes.len() as u64 != verification.reference.total_bytes()
            || sha256(&bytes) != *verification.reference.sha256()
            || seal.byte_len != verification.reference.total_bytes()
            || &seal.sha256 != verification.reference.sha256()
        {
            return Err(JournalError::Invalid(
                "compound source differs from exact native capture and dependencies",
            ));
        }
        self.validate_dependencies(operation)?;
        let previous_progress = object.progress.clone();
        let index = self
            .operations
            .iter()
            .position(|operation| &operation.id == id)
            .expect("checked source owner");
        self.record_verified(proof)?;
        let source = match &mut self.operations[index].action {
            QueueAction::Enqueue { source, .. }
            | QueueAction::Prefix { source, .. }
            | QueueAction::Send { source, .. } => source,
            _ => {
                return Err(JournalError::Invalid(
                    "operation has no typed compound source",
                ));
            }
        };
        let previous =
            std::mem::replace(source, QueueSource::Companion(verification.object.clone()));
        if let Err(error) = preflight(self) {
            match &mut self.operations[index].action {
                QueueAction::Enqueue { source, .. }
                | QueueAction::Prefix { source, .. }
                | QueueAction::Send { source, .. } => *source = previous,
                _ => unreachable!("publication does not change the operation kind"),
            }
            self.objects
                .iter_mut()
                .find(|object| object.id == verification.object)
                .expect("publication retains the object")
                .progress = previous_progress;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn record_take_transfer(
        &mut self,
        id: &QueueOperationId,
        transfer: QueueTransfer,
        attachments: &AttachmentCatalog,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing take owner"))?;
        let QueueAction::Take {
            purpose,
            request,
            phase: QueueTakePhase::Requested | QueueTakePhase::Unknown,
        } = &operation.action
        else {
            return Err(JournalError::Invalid(
                "take cannot accept a replacement transfer",
            ));
        };
        self.validate_transfer(operation, &transfer, attachments, false)?;
        let indexed = QueueIndexedPayload::from_body(
            transfer.items[0].index.bytes(),
            operation.owner.directory(),
            sha256,
        )
        .map_err(|_| JournalError::Invalid("take lacks an authoritative sealed configuration"))?;
        let configuration = QueueConfiguration::capture(&indexed.index().send_config);
        let action = QueueAction::Take {
            purpose: purpose.clone(),
            request: request.clone(),
            phase: QueueTakePhase::Transferring(transfer),
        };
        let index = self
            .operations
            .iter()
            .position(|operation| &operation.id == id)
            .expect("take owner was checked");
        let previous = std::mem::replace(&mut self.operations[index].configuration, configuration);
        if let Err(error) = self.transition(id, action, preflight) {
            self.operations[index].configuration = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn prepare_take_destination(
        &mut self,
        id: &QueueOperationId,
        destination: QueueDestination,
        acknowledge: QueueJson,
        attachments: &AttachmentCatalog,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing take destination owner"))?;
        let QueueAction::Take {
            purpose,
            request,
            phase: QueueTakePhase::Transferring(transfer),
        } = &operation.action
        else {
            return Err(JournalError::Invalid("take has no transferable capture"));
        };
        self.validate_transfer(operation, transfer, attachments, true)?;
        self.validate_destination(operation, purpose, &destination)?;
        validate_directory_body(&acknowledge, &operation.owner)?;
        let reference = match &destination {
            QueueDestination::Draft { companion } => Some(DraftCompanionRef {
                operation: id.clone(),
                companion: companion.clone(),
            }),
            QueueDestination::Send { .. } => None,
        };
        let local = QueueLocalTake {
            transfer: transfer.clone(),
            destination,
            acknowledge,
            send: QueueTakeSendState::Prepared,
        };
        let action = QueueAction::Take {
            purpose: purpose.clone(),
            request: request.clone(),
            phase: QueueTakePhase::LocalOwned(local),
        };
        if let Some(reference) = reference.filter(|_| !self.cleared_takes.contains(id)) {
            self.draft_companions.push(reference);
        }
        if let Err(error) = self.transition(id, action, preflight) {
            self.draft_companions
                .retain(|reference| &reference.operation != id);
            return Err(error);
        }
        Ok(())
    }

    /// Metadata-only eligibility. File verification and durable authority belong to
    /// AttachmentJournal::authorize_queue_acknowledgement, never this live value.
    pub(crate) fn local_take(&self, id: &QueueOperationId) -> Option<&QueueLocalTake> {
        match &self.operation(id)?.action {
            QueueAction::Take {
                phase:
                    QueueTakePhase::LocalOwned(local)
                    | QueueTakePhase::AcknowledgementUnknown(local)
                    | QueueTakePhase::Acknowledged { local, .. },
                ..
            } => Some(local),
            _ => None,
        }
    }

    pub(crate) fn mark_unknown(&mut self, id: &QueueOperationId) -> Result<(), JournalError> {
        let operation = self
            .operations
            .iter_mut()
            .find(|operation| &operation.id == id)
            .ok_or(JournalError::Invalid("missing unknown queue operation"))?;
        match &mut operation.action {
            QueueAction::Enqueue { phase, .. } => match phase {
                QueueEnqueuePhase::Ready { payload, request } => {
                    *phase = QueueEnqueuePhase::Unknown {
                        payload: payload.clone(),
                        request: request.clone(),
                    }
                }
                QueueEnqueuePhase::Unknown { .. } => {}
                _ => return Err(JournalError::Invalid("enqueue was not prepared")),
            },
            QueueAction::Take { phase, .. } => match phase {
                QueueTakePhase::Requested | QueueTakePhase::Unknown => {
                    *phase = QueueTakePhase::Unknown
                }
                QueueTakePhase::LocalOwned(local) => {
                    *phase = QueueTakePhase::AcknowledgementUnknown(local.clone())
                }
                QueueTakePhase::AcknowledgementUnknown(_) => {}
                _ => return Err(JournalError::Invalid("take has no dispatched stage")),
            },
            QueueAction::Prefix { phase, .. } => match phase {
                QueuePrefixPhase::Previewed(handoff) => {
                    *phase = QueuePrefixPhase::ReservationUnknown(handoff.clone())
                }
                QueuePrefixPhase::ReservationUnknown(_) => {}
                _ => return Err(JournalError::Invalid("prefix has no captured reservation")),
            },
            QueueAction::Send { .. } => {
                return Err(JournalError::Invalid(
                    "local sends use preflighted dispatch transitions",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn references_native(&self, id: &AttachmentId) -> bool {
        self.operations.iter().any(|operation| {
            operation
                .native_roots
                .iter()
                .any(|root| root == id.as_str())
                || operation.preparation.as_ref().is_some_and(|preparation| {
                    preparation.files.iter().any(|file| {
                        file.local_attachment_id == id.as_str()
                            || file.document_group.as_deref() == Some(id.as_str())
                    })
                })
                || resource_items_of(&operation.action)
                    .chain(&operation.prefix_downloads)
                    .any(|item| item.files.iter().any(|file| file.local() == id.as_str()))
        })
    }

    pub(crate) fn references_captured_native(&self, id: &AttachmentId) -> bool {
        self.captured_sources
            .iter()
            .filter_map(Weak::upgrade)
            .any(|source| source.native_roots.contains(id))
    }

    pub(crate) fn contains_blob(&self, id: &BlobId) -> bool {
        self.objects.iter().any(|object| {
            object.reference.chunks().iter().any(|chunk| {
                chunk
                    .descriptor()
                    .is_ok_and(|descriptor| descriptor.id() == id)
            })
        }) || self.retired.iter().any(|retired| retired == id.as_str())
    }

    pub(crate) fn holds(&self) -> Vec<QueueChunkHold> {
        self.objects
            .iter()
            .flat_map(|object| {
                (0..object.reference.chunks().len()).filter_map(|ordinal| object.hold(ordinal))
            })
            .collect()
    }

    pub(crate) fn stored_chunks(&self) -> Vec<BlobDescriptor> {
        self.objects
            .iter()
            .flat_map(|object| {
                object
                    .reference
                    .chunks()
                    .iter()
                    .enumerate()
                    .filter(|(ordinal, _)| object.chunk_stored(*ordinal))
                    .filter_map(|(_, chunk)| chunk.descriptor().ok())
            })
            .collect()
    }

    pub(crate) fn retired_blobs(&self) -> Result<Vec<BlobId>, JournalError> {
        self.retired
            .iter()
            .map(|id| {
                BlobId::parse(id).map_err(|_| JournalError::Invalid("invalid retired queue blob"))
            })
            .collect()
    }

    pub(crate) fn local_send_retirement(
        &self,
        id: &QueueOperationId,
    ) -> Option<&QueueLocalSendRetirement> {
        self.local_send_retirements
            .iter()
            .find(|job| &job.operation == id)
    }

    pub(crate) fn local_send_deleted(&self, id: &QueueOperationId) -> bool {
        self.operation(id).is_some_and(|operation| {
            matches!(
                operation.action(),
                QueueAction::Send {
                    phase: QueueSendPhase::Deleted,
                    ..
                }
            )
        }) || self
            .local_send_retirement(id)
            .is_some_and(|job| job.deleted)
    }

    pub(crate) fn local_send_retirements(&self) -> &[QueueLocalSendRetirement] {
        &self.local_send_retirements
    }

    /// Exact captured file obligations, including derived outputs. Cleanup jobs
    /// retain this set after the live operation and its preparation leave.
    pub(crate) fn composer_file_descriptors(
        &self,
        id: &QueueOperationId,
    ) -> Result<Vec<QueueNativeFileDescriptor>, JournalError> {
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing composer file owner"))?;
        let mut files = operation
            .source_intent
            .as_ref()
            .map_or_else(Vec::new, |intent| intent.files().to_vec());
        if let Some(preparation) = &operation.preparation {
            files.extend(
                preparation
                    .files
                    .iter()
                    .map(|file| QueueNativeFileDescriptor {
                        id: file.local_attachment_id.clone(),
                        byte_len: file.byte_len,
                        sha256: file.sha256,
                    }),
            );
        }
        for reference in operation.dependencies() {
            let take = self
                .local_take(&reference.operation)
                .ok_or(JournalError::Invalid(
                    "composer lost its original take files",
                ))?;
            for file in take.transfer.items.iter().flat_map(|item| &item.files) {
                let descriptor = file.descriptor()?;
                files.push(QueueNativeFileDescriptor {
                    id: descriptor.id().as_str().to_owned(),
                    byte_len: descriptor.byte_len(),
                    sha256: *descriptor.sha256(),
                });
            }
        }
        canonical_retirement_files(files)
    }

    /// Commit this continuation before removing the terminal consumer. Its file
    /// set must exactly cover the original native and companion byte owners.
    pub(crate) fn begin_local_send_retirement(
        &mut self,
        id: &QueueOperationId,
        files: Vec<QueueNativeFileDescriptor>,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let files = canonical_retirement_files(files)?;
        if let Some(job) = self.local_send_retirement(id) {
            if job.files != files {
                return Err(JournalError::Invalid(
                    "local send retirement changed its file plan",
                ));
            }
            return preflight(self);
        }
        if self.local_send_retirements.len() >= MAX_ENTRIES {
            return Err(JournalError::Invalid(
                "local send retirement capacity exceeded",
            ));
        }
        let operation = self
            .operation(id)
            .ok_or(JournalError::Invalid("missing local send retirement owner"))?;
        let QueueAction::Send { phase, .. } = operation.action() else {
            return Err(JournalError::Invalid(
                "local send retirement requires a Send consumer",
            ));
        };
        if !phase.terminal() {
            return Err(JournalError::Invalid(
                "only a terminal local send can begin retirement",
            ));
        }
        if files != self.composer_file_descriptors(id)? {
            return Err(JournalError::Invalid(
                "local send retirement differs from captured byte owners",
            ));
        }
        let job = QueueLocalSendRetirement {
            operation: id.clone(),
            owner: operation.owner.clone(),
            dependencies: operation
                .dependencies()
                .iter()
                .map(|reference| reference.operation.clone())
                .collect(),
            files,
            deleted: matches!(phase, QueueSendPhase::Deleted),
        };
        self.local_send_retirements.push(job);
        if let Err(error) = preflight(self) {
            self.local_send_retirements.pop();
            return Err(error);
        }
        Ok(())
    }

    /// The caller has committed edit/Take cleanup and attachment deletion intents.
    /// Independently retained draft or consumer roots keep their own obligation.
    pub(crate) fn finish_local_send_retirement(
        &mut self,
        id: &QueueOperationId,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let index = self
            .local_send_retirements
            .iter()
            .position(|job| &job.operation == id)
            .ok_or(JournalError::Invalid(
                "missing local send cleanup continuation",
            ))?;
        if self.operation(id).is_some() {
            return Err(JournalError::Invalid(
                "local send consumer still owns its cleanup",
            ));
        }
        for dependency in &self.local_send_retirements[index].dependencies {
            if self.operation(dependency).is_some()
                && !self.companion_has_external_owner(dependency)
            {
                return Err(JournalError::Invalid(
                    "local send cleanup still has an unowned take",
                ));
            }
        }
        let job = self.local_send_retirements.remove(index);
        if let Err(error) = preflight(self) {
            self.local_send_retirements.insert(index, job);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn companion_has_external_owner(&self, id: &QueueOperationId) -> bool {
        self.draft_companions
            .iter()
            .any(|reference| &reference.operation == id)
            || self
                .composer_edits
                .iter()
                .flat_map(|record| &record.versions)
                .any(|edit| {
                    &edit.operation == id
                        || edit
                            .companions
                            .iter()
                            .any(|reference| &reference.operation == id)
                })
            || self.operations.iter().any(|operation| {
                operation
                    .dependencies()
                    .iter()
                    .any(|reference| &reference.operation == id)
            })
    }

    pub(crate) fn inline_sources(&self) -> impl Iterator<Item = &QueueNativeSource> {
        self.operations
            .iter()
            .filter_map(|operation| match operation.source() {
                Some(QueueSource::Inline(source)) => Some(source.as_ref()),
                _ => None,
            })
    }

    pub(crate) fn byte_authority(&self) -> QueueByteAuthority {
        let mut authority = QueueByteAuthority {
            objects: self.objects.clone(),
            holds: self.holds(),
            stored: self.stored_chunks(),
            retired: self
                .retired_blobs()
                .expect("only a validated queue grants byte authority"),
            ..QueueByteAuthority::default()
        };
        for operation in &self.operations {
            if let Ok(binding) = self.consumer_edit_binding(operation.id()) {
                authority.consumer_edits.push(binding);
            }
            authority
                .native_roots
                .extend(operation.native_roots.iter().cloned());
            if let Some(preparation) = &operation.preparation {
                for file in &preparation.files {
                    authority
                        .native_roots
                        .insert(file.local_attachment_id.clone());
                    authority
                        .native_roots
                        .extend(file.document_group.iter().cloned());
                }
            }
            for item in resource_items_of(&operation.action).chain(&operation.prefix_downloads) {
                authority
                    .native_roots
                    .extend(item.files.iter().map(|file| file.local().to_owned()));
            }
            let Some(local) = self.local_take(&operation.id) else {
                continue;
            };
            let mut objects: Vec<_> = local
                .transfer
                .items
                .iter()
                .map(|item| item.payload.clone())
                .collect();
            match &local.destination {
                QueueDestination::Draft { companion } => objects.push(companion.clone()),
                QueueDestination::Send {
                    companion, prompt, ..
                } => {
                    objects.push(companion.clone());
                    objects.push(prompt.clone());
                }
            }
            authority
                .acknowledgements
                .push(QueueAcknowledgementAuthority {
                    operation: operation.id.clone(),
                    directory: operation.owner.directory().to_owned(),
                    request: local.acknowledge.clone(),
                    receipt: local.transfer.receipt.clone(),
                    objects,
                    items: local.transfer.items.clone(),
                });
        }
        authority
    }

    /// Explicit terminal handoff. The caller must already have committed any new
    /// owner outside this ledger. A canonical preflight can refuse without loss.
    pub(crate) fn retire_operation(
        &mut self,
        id: &QueueOperationId,
        preflight: impl FnOnce(&Self) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        let index = self
            .operations
            .iter()
            .position(|operation| &operation.id == id)
            .ok_or(JournalError::Invalid("missing queue retirement owner"))?;
        if self
            .captured_sources
            .iter()
            .filter_map(Weak::upgrade)
            .any(|capture| {
                capture
                    .companions
                    .iter()
                    .any(|reference| &reference.operation == id)
            })
        {
            return Err(JournalError::Invalid(
                "registered capture still owns its original Take",
            ));
        }
        if self
            .draft_companions
            .iter()
            .any(|reference| &reference.operation == id)
        {
            return Err(JournalError::Invalid(
                "composer still owns its queue companion",
            ));
        }
        if self
            .composer_edits
            .iter()
            .flat_map(|record| &record.versions)
            .any(|edit| {
                &edit.operation == id
                    || edit
                        .companions
                        .iter()
                        .any(|reference| &reference.operation == id)
            })
        {
            return Err(JournalError::Invalid(
                "compound editing snapshot still owns this source",
            ));
        }
        if self.operations.iter().any(|operation| {
            operation
                .dependencies()
                .iter()
                .any(|reference| &reference.operation == id)
        }) {
            return Err(JournalError::Invalid(
                "another operation retains this original take companion",
            ));
        }
        if matches!(&self.operations[index].action, QueueAction::Take {
            purpose: QueueTakePurpose::Send { .. }, phase: QueueTakePhase::Acknowledged { local, .. }, ..
        } if local.send != QueueTakeSendState::Admitted)
        {
            return Err(JournalError::Invalid(
                "queued send has no authoritative admission",
            ));
        }
        let terminal = matches!(
            &self.operations[index].action,
            QueueAction::Enqueue {
                phase: QueueEnqueuePhase::Accepted { .. },
                ..
            } | QueueAction::Take {
                phase: QueueTakePhase::Acknowledged { .. },
                ..
            } | QueueAction::Prefix {
                phase: QueuePrefixPhase::Admitted(_),
                ..
            } | QueueAction::Send {
                phase: QueueSendPhase::Admitted { .. } | QueueSendPhase::Deleted,
                ..
            }
        );
        if !terminal {
            return Err(JournalError::Invalid(
                "unresolved queue ownership cannot retire",
            ));
        }
        let additions: Vec<_> = self
            .objects
            .iter()
            .filter(|object| &object.operation == id)
            .flat_map(|object| object.reference.chunks().iter())
            .map(|chunk| {
                chunk
                    .descriptor()
                    .map(|descriptor| descriptor.id().as_str().to_owned())
            })
            .collect::<Result<_, _>>()?;
        if self.retired.len().saturating_add(additions.len()) > MAX_ENTRIES {
            return Err(JournalError::Invalid("queue retirement capacity exceeded"));
        }
        let previous_objects = std::mem::take(&mut self.objects);
        self.objects = previous_objects
            .iter()
            .filter(|object| &object.operation != id)
            .cloned()
            .collect();
        let operation = self.operations.remove(index);
        let previous_cleared = self.cleared_takes.clone();
        self.cleared_takes.retain(|cleared| cleared != id);
        let old_retired = self.retired.len();
        self.retired.extend(additions);
        if let Err(error) = preflight(self) {
            self.retired.truncate(old_retired);
            self.operations.insert(index, operation);
            self.cleared_takes = previous_cleared;
            self.objects = previous_objects;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn record_released(
        &mut self,
        id: &BlobId,
        commit: SpoolCommit,
    ) -> Result<(), JournalError> {
        if commit != SpoolCommit::Durable
            || !self.retired.iter().any(|retired| retired == id.as_str())
        {
            return Err(JournalError::Invalid(
                "queue release is not a durable retirement",
            ));
        }
        self.retired.retain(|retired| retired != id.as_str());
        Ok(())
    }

    pub(crate) fn validate<'a>(
        &'a self,
        attachments: &AttachmentCatalog,
        preparations: &[crate::attachment_catalog::PreparationHold],
        retired_attachments: &[BlobId],
        messages: &mut HashSet<(&'a str, &'a str)>,
    ) -> Result<(), JournalError> {
        self.validate_source_claims()?;
        self.validate_context_reservations()?;
        if self.local_send_retirements.len() > MAX_ENTRIES {
            return Err(JournalError::Invalid(
                "too many local send cleanup continuations",
            ));
        }
        let mut jobs = HashSet::new();
        for job in &self.local_send_retirements {
            QueueOperationId::new(job.operation.as_str())?;
            job.owner.validate()?;
            if !jobs.insert(&job.operation)
                || job.dependencies.len() > MAX_ENTRIES
                || canonical_retirement_files(job.files.clone())? != job.files
            {
                return Err(JournalError::Invalid(
                    "invalid local send cleanup continuation",
                ));
            }
            let mut dependencies = HashSet::new();
            for id in &job.dependencies {
                QueueOperationId::new(id.as_str())?;
                if id == &job.operation || !dependencies.insert(id) {
                    return Err(JournalError::Invalid(
                        "invalid local send cleanup dependency",
                    ));
                }
                if let Some(take) = self.operation(id)
                    && (take.owner.draft_key()? != job.owner.draft_key()?
                        || take.owner.directory != job.owner.directory
                        || !matches!(
                            take.action(),
                            QueueAction::Take {
                                purpose: QueueTakePurpose::Edit,
                                phase: QueueTakePhase::Acknowledged { .. },
                                ..
                            }
                        ))
                {
                    return Err(JournalError::Invalid(
                        "local send cleanup changed its original take owner",
                    ));
                }
            }
            if let Some(operation) = self.operation(&job.operation) {
                if operation.owner.draft_key()? != job.owner.draft_key()?
                    || operation.owner.directory != job.owner.directory
                    || !matches!(operation.action(), QueueAction::Send { phase, .. } if phase.terminal()
                        && job.deleted == matches!(phase, QueueSendPhase::Deleted))
                    || operation
                        .dependencies()
                        .iter()
                        .map(|reference| &reference.operation)
                        .ne(job.dependencies.iter())
                    || job.files != self.composer_file_descriptors(&job.operation)?
                {
                    return Err(JournalError::Invalid(
                        "local send cleanup differs from its terminal consumer",
                    ));
                }
            } else if job.deleted
                && preparations.iter().any(|hold| {
                    job.owner
                        .draft_key()
                        .as_ref()
                        .is_ok_and(|owner| owner == &hold.owner)
                        && job.files.iter().any(|file| {
                            file.id == hold.id.as_str() && file.id == hold.blob.as_str()
                        })
                })
            {
                return Err(JournalError::Invalid(
                    "deleted consumer removal must retire its exact preparation holds atomically",
                ));
            }
            for file in &job.files {
                if attachments.get(file.id.as_str()).is_some_and(|held| {
                    held.blob.byte_len() != file.byte_len || held.blob.sha256() != &file.sha256
                }) {
                    return Err(JournalError::Invalid(
                        "local send cleanup changed its captured file descriptor",
                    ));
                }
            }
        }
        if self.composer_edits.len() > MAX_ENTRIES
            || self.cleared_takes.len() > MAX_ENTRIES
            || self
                .composer_edits
                .iter()
                .map(|record| record.versions.len())
                .sum::<usize>()
                > MAX_ENTRIES
        {
            return Err(JournalError::Invalid(
                "compound edit ownership exceeds its count limit",
            ));
        }
        let mut owners = HashSet::new();
        for record in &self.composer_edits {
            let owner = record.owner.clone().into_key()?;
            if !owners.insert(owner.clone())
                || record
                    .current
                    .as_ref()
                    .is_some_and(|current| !record.versions.contains(current))
            {
                return Err(JournalError::Invalid("invalid compound edit owner"));
            }
            let mut versions = HashSet::new();
            for edit in &record.versions {
                if !versions.insert(&edit.object) {
                    return Err(JournalError::Invalid("duplicate compound editing object"));
                }
                self.validate_composer_edit(&owner, edit, record.current.as_ref() == Some(edit))?;
            }
        }
        let mut cleared = HashSet::new();
        for id in &self.cleared_takes {
            if !cleared.insert(id)
                || !matches!(
                    self.operation(id).map(|operation| operation.action()),
                    Some(QueueAction::Take {
                        purpose: QueueTakePurpose::Edit,
                        ..
                    })
                )
            {
                return Err(JournalError::Invalid("invalid cleared compound source"));
            }
        }
        if self.operations.len() > MAX_ENTRIES
            || self.objects.len() > MAX_ENTRIES
            || self.retired.len() > MAX_ENTRIES
        {
            return Err(JournalError::Invalid(
                "queue ownership count limit exceeded",
            ));
        }
        let mut operations = HashSet::new();
        let mut take_contexts = HashSet::new();
        for (runtime, context) in self.retired_first_use_contexts()? {
            take_contexts.insert((runtime, context));
        }
        if self.draft_companions.len() > MAX_ENTRIES {
            return Err(JournalError::Invalid(
                "composer companion count limit exceeded",
            ));
        }
        let mut companions = HashSet::new();
        for reference in &self.draft_companions {
            let operation = self
                .operation(&reference.operation)
                .ok_or(JournalError::Invalid(
                    "composer companion has no operation owner",
                ))?;
            if !companions.insert(&reference.operation)
                || !matches!(self.local_take(&reference.operation),
                Some(QueueLocalTake { destination: QueueDestination::Draft { companion }, .. })
                    if companion == &reference.companion)
                || !matches!(
                    operation.action(),
                    QueueAction::Take {
                        purpose: QueueTakePurpose::Edit,
                        ..
                    }
                )
            {
                return Err(JournalError::Invalid(
                    "invalid composer companion ownership",
                ));
            }
        }
        let mut retained_roots = HashSet::new();
        for operation in &self.operations {
            QueueOperationId::new(operation.id.as_str())?;
            operation.owner.validate()?;
            operation.configuration.validate()?;
            self.validate_source_metadata(operation, attachments)?;
            self.validate_dependencies(operation)?;
            self.validate_composer_preparation(
                operation,
                attachments,
                preparations,
                retired_attachments,
            )?;
            if !operation.prefix_downloads.is_empty() {
                self.validate_prefix_downloads(operation, &operation.prefix_downloads)?;
                for item in &operation.prefix_downloads {
                    self.validate_item(operation, item, attachments, false)?;
                }
            }
            if !operations.insert(&operation.id) {
                return Err(JournalError::Invalid("duplicate queue operation"));
            }
            let mut roots = HashSet::new();
            for root in &operation.native_roots {
                identifier(root)?;
                if !roots.insert(root) || retired_attachments.iter().any(|id| id.as_str() == root) {
                    return Err(JournalError::Invalid("conflicting native queue root"));
                }
                retained_roots.insert(root.as_str());
            }
            if operation
                .prepared_roots
                .iter()
                .any(|root| !roots.contains(root))
            {
                return Err(JournalError::Invalid(
                    "prepared queue source is not a retained native root",
                ));
            }
            for item in resource_items_of(&operation.action).chain(&operation.prefix_downloads) {
                retained_roots.extend(item.files.iter().map(QueueFileBinding::local));
            }
            if let Some(preparation) = &operation.preparation {
                for file in &preparation.files {
                    retained_roots.insert(file.local_attachment_id.as_str());
                    retained_roots.extend(file.document_group.as_deref());
                }
            }
            let resources: Vec<_> = operation
                .prepared_roots
                .iter()
                .cloned()
                .map(AttachmentId::from)
                .collect();
            crate::attachment_catalog::validate_resources(
                &mut 0,
                attachments,
                &operation.owner.draft_key()?,
                &resources,
            )?;
            if let Some(source) = operation.source() {
                match source {
                    QueueSource::Inline(source) => {
                        if source.owner != operation.owner.draft_key()?
                            || native_roots(source) != operation.native_roots
                            || prepared_roots(source) != operation.prepared_roots
                        {
                            return Err(JournalError::Invalid(
                                "inline queue source or roots differ",
                            ));
                        }
                        crate::draft_storage::validate(&mut 0, &source.draft)?;
                        crate::attachment_catalog::validate_draft(
                            attachments,
                            &source.owner,
                            &source.draft,
                            &mut HashMap::new(),
                        )?;
                    }
                    QueueSource::Companion(id) => {
                        self.require_object(operation, id, QueueObjectKind::NativeCompanion, true)?;
                    }
                }
            }
            match &operation.action {
                QueueAction::Enqueue { source, phase } => {
                    for context in self.source_context_ids(operation)? {
                        if !take_contexts.insert((operation.owner.runtime.clone(), context)) {
                            return Err(JournalError::Invalid(
                                "duplicate captured enqueue context identity",
                            ));
                        }
                    }
                    match phase {
                        QueueEnqueuePhase::Capturing => {}
                        QueueEnqueuePhase::Ready { payload, request }
                        | QueueEnqueuePhase::Unknown { payload, request }
                        | QueueEnqueuePhase::Accepted {
                            payload, request, ..
                        }
                        | QueueEnqueuePhase::RetryPrepared {
                            payload, request, ..
                        } => {
                            if matches!(source, QueueSource::Inline(_)) {
                                return Err(JournalError::Invalid(
                                    "enqueue has no durable source companion",
                                ));
                            }
                            self.require_composer_inputs(operation)?;
                            self.validate_enqueue_request(operation, payload, request)?;
                            if let QueueEnqueuePhase::Accepted { receipt, .. }
                            | QueueEnqueuePhase::RetryPrepared { receipt, .. } = phase
                            {
                                validate_receipt(receipt, operation, "enqueue")?;
                                self.validate_enqueue_preparation_receipt(operation, receipt)?;
                                let body = receipt.value()?;
                                if let Some(message) = body
                                    .get("receipt")
                                    .and_then(|receipt| receipt.get("messageId"))
                                    .and_then(serde_json::Value::as_str)
                                    && !take_contexts.insert((
                                        operation.owner.runtime.clone(),
                                        message.to_owned(),
                                    ))
                                {
                                    return Err(JournalError::Invalid(
                                        "enqueue prompt aliases another retained identity",
                                    ));
                                }
                            }
                            if let QueueEnqueuePhase::RetryPrepared { retry, receipt, .. } = phase {
                                validate_enqueue_retry(retry, receipt, &operation.owner)?;
                            }
                        }
                    }
                }
                QueueAction::Take {
                    purpose,
                    request,
                    phase,
                } => {
                    validate_directory_body(request, &operation.owner)?;
                    let body = request.value()?;
                    let ids = body
                        .get("itemIds")
                        .and_then(serde_json::Value::as_array)
                        .ok_or(JournalError::Invalid("take request has no item identities"))?;
                    let mut unique = HashSet::new();
                    if body.get("format").and_then(serde_json::Value::as_str) != Some("payload-v1")
                        || ids.is_empty()
                        || ids.len() > 20
                        || ids.iter().any(|id| {
                            id.as_str()
                                .is_none_or(|id| identifier(id).is_err() || !unique.insert(id))
                        })
                    {
                        return Err(JournalError::Invalid(
                            "invalid retained payload take request",
                        ));
                    }
                    if let QueueTakePurpose::Send { message, .. } = purpose {
                        reserve_message(messages, operation, message)?;
                    }
                    if let Some(transfer) = transfer_of(&operation.action) {
                        let complete = matches!(
                            phase,
                            QueueTakePhase::LocalOwned(_)
                                | QueueTakePhase::AcknowledgementUnknown(_)
                                | QueueTakePhase::Acknowledged { .. }
                        );
                        self.validate_transfer(operation, transfer, attachments, complete)?;
                        if matches!(purpose, QueueTakePurpose::Send { .. }) {
                            for item in &transfer.items {
                                let index = QueueIndexedPayload::from_body(
                                    item.index.bytes(),
                                    operation.owner.directory(),
                                    sha256,
                                )
                                .map_err(|_| {
                                    JournalError::Invalid("invalid queued send context index")
                                })?;
                                for context in &index.index().context_ids {
                                    if !take_contexts.insert((
                                        operation.owner.runtime.clone(),
                                        context.to_string(),
                                    )) {
                                        return Err(JournalError::Invalid(
                                            "duplicate queue context admission identity",
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    let local = match phase {
                        QueueTakePhase::LocalOwned(local)
                        | QueueTakePhase::AcknowledgementUnknown(local)
                        | QueueTakePhase::Acknowledged { local, .. } => Some(local),
                        _ => None,
                    };
                    if let Some(local) = local {
                        self.validate_destination(operation, purpose, &local.destination)?;
                        validate_directory_body(&local.acknowledge, &operation.owner)?;
                    }
                    if let QueueTakePhase::Acknowledged { receipt, .. } = phase {
                        validate_receipt(receipt, operation, "take")?;
                    }
                }
                QueueAction::Prefix {
                    message,
                    source,
                    request,
                    phase,
                } => {
                    reserve_message(messages, operation, message)?;
                    match phase {
                        QueuePrefixPhase::Capturing => {
                            if operation.source_intent.is_some() {
                                for context in self.source_context_ids(operation)? {
                                    if !take_contexts
                                        .insert((operation.owner.runtime.clone(), context))
                                    {
                                        return Err(JournalError::Invalid(
                                            "duplicate captured companion context identity",
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {
                            validate_real_prefix_request(request, &operation.owner, message)?;
                        }
                    }
                    if let QueuePrefixPhase::Previewed(handoff)
                    | QueuePrefixPhase::ReservationUnknown(handoff)
                    | QueuePrefixPhase::Reserved(handoff)
                    | QueuePrefixPhase::Admitted(handoff) = phase
                    {
                        if matches!(source, QueueSource::Inline(_)) {
                            return Err(JournalError::Invalid(
                                "prefix has no durable composer source",
                            ));
                        }
                        self.validate_prefix(operation, message, request, handoff)?;
                        self.require_composer_inputs(operation)?;
                        self.require_object(
                            operation,
                            &handoff.prompt,
                            QueueObjectKind::FrozenSend,
                            true,
                        )?;
                        for item in &handoff.items {
                            self.validate_item(operation, item, attachments, true)?;
                        }
                        if let Some(composer) = &handoff.composer_item {
                            self.validate_item(operation, composer, attachments, true)?;
                        }
                        for context in &handoff.context_ids {
                            reserve_message(messages, operation, context)?;
                        }
                    }
                }
                QueueAction::Send {
                    message,
                    source,
                    context_ids,
                    phase,
                    ..
                } => {
                    reserve_message(messages, operation, message)?;
                    if context_ids != &self.source_context_ids(operation)? {
                        return Err(JournalError::Invalid(
                            "local send context order differs from its captured sources",
                        ));
                    }
                    for context in context_ids {
                        reserve_message(messages, operation, context)?;
                    }
                    if let QueueSendPhase::Prepared { frame }
                    | QueueSendPhase::Unknown { frame }
                    | QueueSendPhase::Admitted { frame } = phase
                    {
                        if !matches!(source, QueueSource::Companion(_)) {
                            return Err(JournalError::Invalid(
                                "local send has no complete original source companion",
                            ));
                        }
                        self.require_object(operation, frame, QueueObjectKind::FrozenSend, true)?;
                        self.require_composer_inputs(operation)?;
                    }
                }
            }
        }
        if messages.iter().any(|(runtime, message)| {
            take_contexts.contains(&(runtime.to_string(), message.to_string()))
        }) {
            return Err(JournalError::Invalid(
                "queue context identity aliases another retained admission",
            ));
        }
        let mut objects = HashSet::new();
        let mut blobs = HashSet::new();
        for object in &self.objects {
            QueueObjectId::new(object.id.as_str())?;
            object.reference.validate(object.kind)?;
            if !operations.contains(&object.operation)
                || !objects.insert(&object.id)
                || object.generation == 0
            {
                return Err(JournalError::Invalid("dangling or duplicate queue object"));
            }
            if let QueueObjectProgress::Writing { durable_ordinals } = &object.progress {
                let mut ordinals = HashSet::new();
                for ordinal in durable_ordinals {
                    if *ordinal >= object.reference.chunks().len() || !ordinals.insert(ordinal) {
                        return Err(JournalError::Invalid("invalid durable queue chunk ordinal"));
                    }
                }
            }
            for chunk in object.reference.chunks() {
                let descriptor = chunk.descriptor()?;
                if !blobs.insert(*descriptor.id())
                    || attachments
                        .values()
                        .any(|file| file.blob.id() == descriptor.id())
                    || preparations
                        .iter()
                        .any(|hold| &hold.blob == descriptor.id())
                    || retired_attachments.contains(descriptor.id())
                    || retained_roots.contains(descriptor.id().as_str())
                {
                    return Err(JournalError::Invalid(
                        "queue chunk aliases another local owner",
                    ));
                }
            }
        }
        let mut retired = HashSet::new();
        for id in self.retired_blobs()? {
            if blobs.contains(&id)
                || !retired.insert(id)
                || attachments.values().any(|file| file.blob.id() == &id)
                || preparations.iter().any(|hold| hold.blob == id)
                || retired_attachments.contains(&id)
                || retained_roots.contains(id.as_str())
            {
                return Err(JournalError::Invalid(
                    "retired queue blob remains owned or duplicated",
                ));
            }
        }
        Ok(())
    }

    fn require_object(
        &self,
        operation: &QueueOperation,
        id: &QueueObjectId,
        kind: QueueObjectKind,
        verified: bool,
    ) -> Result<&QueueObject, JournalError> {
        self.object(id)
            .filter(|object| {
                object.operation == operation.id
                    && object.kind == kind
                    && (!verified || object.verified())
            })
            .ok_or(JournalError::Invalid(
                "queue root lacks its exact owned capture",
            ))
    }

    /// Dependencies can only point to original Edit takes. They have no outgoing
    /// dependencies themselves, so self references and cycles cannot be admitted.
    fn validate_dependencies(&self, operation: &QueueOperation) -> Result<(), JournalError> {
        if operation.dependencies().len() > MAX_ENTRIES {
            return Err(JournalError::Invalid(
                "too many original companion dependencies",
            ));
        }
        let mut unique = HashSet::new();
        for reference in operation.dependencies() {
            QueueOperationId::new(reference.operation.as_str())?;
            QueueObjectId::new(reference.companion.as_str())?;
            let original = self
                .operation(&reference.operation)
                .ok_or(JournalError::Invalid(
                    "companion dependency lost its original take",
                ))?;
            if reference.operation == operation.id
                || !unique.insert(&reference.operation)
                || original.source_intent.is_some()
                || original.owner.runtime != operation.owner.runtime
                || original.owner.directory != operation.owner.directory
                || original.owner.session != operation.owner.session
                || original.owner.draft_key()? != operation.owner.draft_key()?
                || !matches!(&original.action, QueueAction::Take { purpose: QueueTakePurpose::Edit,
                    phase: QueueTakePhase::Acknowledged { local: QueueLocalTake {
                        destination: QueueDestination::Draft { companion }, .. }, .. }, .. }
                    if companion == &reference.companion)
            {
                return Err(JournalError::Invalid(
                    "invalid original take companion dependency",
                ));
            }
            self.require_object(
                original,
                &reference.companion,
                QueueObjectKind::NativeCompanion,
                true,
            )?;
        }
        Ok(())
    }

    fn original_source_occurrences(
        &self,
        operation: &QueueOperation,
    ) -> Result<Vec<(QueueSourceOccurrence, Vec<String>)>, JournalError> {
        self.validate_dependencies(operation)?;
        if operation.dependencies().is_empty() {
            return Ok(Vec::new());
        }
        let source_sha256 = *operation.source_sha256().ok_or(JournalError::Invalid(
            "source occurrences have no original consumer seal",
        ))?;
        let mut originals = Vec::new();
        for reference in operation.dependencies() {
            let local = self
                .local_take(&reference.operation)
                .ok_or(JournalError::Invalid("missing original companion take"))?;
            for (item_ordinal, item) in local.transfer.items.iter().enumerate() {
                if originals.len() >= MAX_ENTRIES {
                    return Err(JournalError::Invalid(
                        "too many companion source occurrences",
                    ));
                }
                let indexed = QueueIndexedPayload::from_body(
                    item.index.bytes(),
                    operation.owner.directory(),
                    sha256,
                )
                .map_err(|_| JournalError::Invalid("invalid original companion context index"))?;
                originals.push((
                    QueueSourceOccurrence {
                        source_index: originals.len() + 1,
                        take: reference.operation.clone(),
                        item_ordinal,
                        source_sha256,
                        payload_sha256: indexed.pointer().sha256(),
                        index_sha256: indexed.index_hash(),
                        context_rebinding: None,
                    },
                    indexed
                        .index()
                        .context_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                ));
            }
        }
        Ok(originals)
    }

    fn occurrence_context_ids(
        &self,
        operation: &QueueOperation,
    ) -> Result<Vec<Vec<String>>, JournalError> {
        let originals = self.original_source_occurrences(operation)?;
        let Some(occurrences) = &operation.source_occurrences else {
            if operation.dependencies().iter().any(|reference| {
                self.operation(&reference.operation)
                    .is_some_and(|take| take.take_source_use.is_some())
            }) {
                return Err(JournalError::Invalid(
                    "known Take claim lost its immutable occurrence table",
                ));
            }
            return Ok(originals.into_iter().map(|(_, ids)| ids).collect());
        };
        if operation.source_intent.is_none()
            || originals.is_empty()
            || occurrences.len() != originals.len()
            || occurrences.len() > MAX_ENTRIES
        {
            return Err(JournalError::Invalid(
                "source occurrence table changed its complete source order",
            ));
        }
        let original_ids: HashSet<_> = originals
            .iter()
            .flat_map(|(_, ids)| ids.iter().map(String::as_str))
            .collect();
        let mut effective = Vec::with_capacity(occurrences.len());
        for (occurrence, (expected, ids)) in occurrences.iter().zip(&originals) {
            if occurrence.source_index != expected.source_index
                || occurrence.take != expected.take
                || occurrence.item_ordinal != expected.item_ordinal
                || occurrence.source_sha256 != expected.source_sha256
                || occurrence.payload_sha256 != expected.payload_sha256
                || occurrence.index_sha256 != expected.index_sha256
            {
                return Err(JournalError::Invalid(
                    "source occurrence differs from its captured source or original index",
                ));
            }
            let Some(QueueTakeSourceUse::Claimed { consumer }) = self
                .operation(&occurrence.take)
                .and_then(QueueOperation::take_source_use)
            else {
                return Err(JournalError::Invalid(
                    "source occurrence has no proven original Take claim",
                ));
            };
            let ids = match &occurrence.context_rebinding {
                None if consumer == &operation.id => ids.clone(),
                Some(rebound) if consumer != &operation.id && rebound.len() == ids.len() => {
                    validate_context_ids(rebound)?;
                    if rebound.iter().any(|id| original_ids.contains(id.as_str())) {
                        return Err(JournalError::Invalid(
                            "repeated source aliases an original context identity",
                        ));
                    }
                    rebound.clone()
                }
                _ => {
                    return Err(JournalError::Invalid(
                        "source occurrence changed first-use or repeated context ownership",
                    ));
                }
            };
            effective.push(ids);
        }
        Ok(effective)
    }

    /// Read the already admitted working IDs for one native-first companion index.
    /// Original sealed payload/index bytes must still be materialized separately.
    pub(crate) fn companion_context_ids(
        &self,
        consumer: &QueueOperationId,
        source_index: usize,
    ) -> Result<Vec<String>, JournalError> {
        let operation = self.operation(consumer).ok_or(JournalError::Invalid(
            "missing companion occurrence consumer",
        ))?;
        let index = source_index.checked_sub(1).ok_or(JournalError::Invalid(
            "native source is not a companion occurrence",
        ))?;
        self.occurrence_context_ids(operation)?
            .get(index)
            .cloned()
            .ok_or(JournalError::Invalid(
                "missing companion occurrence source index",
            ))
    }

    fn dependency_context_ids(
        &self,
        operation: &QueueOperation,
    ) -> Result<Vec<String>, JournalError> {
        Ok(self
            .occurrence_context_ids(operation)?
            .into_iter()
            .flatten()
            .collect())
    }

    fn validate_source_claims(&self) -> Result<(), JournalError> {
        let mut consumers = HashMap::new();
        for operation in &self.operations {
            self.occurrence_context_ids(operation)?;
            for reference in operation.dependencies() {
                let modern = operation.source_occurrences.is_some();
                if consumers
                    .insert(&reference.operation, modern)
                    .is_some_and(|previous| !previous || !modern)
                {
                    return Err(JournalError::Invalid(
                        "legacy original Take has multiple consumers",
                    ));
                }
            }
        }
        for operation in &self.operations {
            let Some(usage) = &operation.take_source_use else {
                continue;
            };
            if !matches!(
                operation.action(),
                QueueAction::Take {
                    purpose: QueueTakePurpose::Edit,
                    ..
                }
            ) {
                return Err(JournalError::Invalid(
                    "source-use marker belongs to a non-Edit operation",
                ));
            }
            match usage {
                QueueTakeSourceUse::Unused => {
                    if consumers.contains_key(&operation.id) {
                        return Err(JournalError::Invalid(
                            "unused original Take already has a consumer",
                        ));
                    }
                }
                QueueTakeSourceUse::Claimed { consumer } => {
                    QueueOperationId::new(consumer.as_str())?;
                    if consumer == &operation.id
                        || !matches!(
                            operation.action(),
                            QueueAction::Take {
                                phase: QueueTakePhase::Acknowledged { .. },
                                ..
                            }
                        )
                    {
                        return Err(JournalError::Invalid(
                            "original Take claim has no acknowledged source",
                        ));
                    }
                    if let Some(first) = self.operation(consumer)
                        && (!first
                            .dependencies()
                            .iter()
                            .any(|reference| reference.operation == operation.id)
                            || first.source_occurrences.is_none())
                    {
                        return Err(JournalError::Invalid(
                            "original Take claim changed its first consumer",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// A retired first consumer cannot make the original source IDs fresh again.
    fn retired_first_use_contexts(&self) -> Result<Vec<(String, String)>, JournalError> {
        let mut reserved = Vec::new();
        for take in &self.operations {
            let Some(QueueTakeSourceUse::Claimed { consumer }) = &take.take_source_use else {
                continue;
            };
            if self.operation(consumer).is_some() {
                continue;
            }
            let local = self.local_take(&take.id).ok_or(JournalError::Invalid(
                "claimed source lost its original Take",
            ))?;
            for item in &local.transfer.items {
                let index = QueueIndexedPayload::from_body(
                    item.index.bytes(),
                    take.owner.directory(),
                    sha256,
                )
                .map_err(|_| JournalError::Invalid("spent source has an invalid original index"))?;
                reserved.extend(
                    index
                        .index()
                        .context_ids
                        .iter()
                        .map(|id| (take.owner.runtime.clone(), id.to_string())),
                );
            }
        }
        Ok(reserved)
    }

    fn source_context_ids(&self, operation: &QueueOperation) -> Result<Vec<String>, JournalError> {
        let mut contexts = operation
            .source_intent
            .as_ref()
            .map_or_else(Vec::new, |intent| intent.native_context_ids().to_vec());
        if let Some(preparation) = &operation.preparation {
            contexts.extend(preparation.derived_context_ids.iter().cloned());
        }
        contexts.extend(self.dependency_context_ids(operation)?);
        validate_context_ids(&contexts)?;
        Ok(contexts)
    }

    /// Every operation reserves its IDs while still Capturing. A prefix's full
    /// handoff replaces its composer-only reservation once the preview exists.
    fn validate_context_reservations(&self) -> Result<(), JournalError> {
        let mut identities = HashSet::new();
        for operation in &self.operations {
            let mut ids = self.source_context_ids(operation)?;
            match &operation.action {
                QueueAction::Send { message, .. } => ids.push(message.clone()),
                QueueAction::Enqueue {
                    phase:
                        QueueEnqueuePhase::Accepted { receipt, .. }
                        | QueueEnqueuePhase::RetryPrepared { receipt, .. },
                    ..
                } => {
                    let body = receipt.value()?;
                    if let Some(message) = body
                        .get("receipt")
                        .and_then(|receipt| receipt.get("messageId"))
                        .and_then(serde_json::Value::as_str)
                    {
                        ids.push(message.to_owned());
                    }
                }
                QueueAction::Prefix { message, phase, .. } => {
                    if let QueuePrefixPhase::Previewed(handoff)
                    | QueuePrefixPhase::ReservationUnknown(handoff)
                    | QueuePrefixPhase::Reserved(handoff)
                    | QueuePrefixPhase::Admitted(handoff) = phase
                    {
                        ids = handoff.context_ids.clone();
                    }
                    ids.push(message.clone());
                }
                QueueAction::Take {
                    purpose: QueueTakePurpose::Send { message, .. },
                    ..
                } => {
                    ids.push(message.clone());
                    if let Some(transfer) = transfer_of(&operation.action) {
                        for item in &transfer.items {
                            let index = QueueIndexedPayload::from_body(
                                item.index.bytes(),
                                operation.owner.directory(),
                                sha256,
                            )
                            .map_err(|_| JournalError::Invalid("invalid queued context index"))?;
                            ids.extend(index.index().context_ids.iter().map(ToString::to_string));
                        }
                    }
                }
                _ => {}
            }
            validate_context_ids(&ids)?;
            for id in ids {
                if !identities.insert((operation.owner.runtime.clone(), id)) {
                    return Err(JournalError::Invalid(
                        "queue admission identity aliases another operation",
                    ));
                }
            }
        }
        for identity in self.retired_first_use_contexts()? {
            if !identities.insert(identity) {
                return Err(JournalError::Invalid(
                    "spent original source context identity remains reserved",
                ));
            }
        }
        Ok(())
    }

    fn require_composer_inputs(&self, operation: &QueueOperation) -> Result<(), JournalError> {
        if operation
            .preparation
            .as_ref()
            .is_some_and(|preparation| preparation.inputs_object.is_none())
        {
            return Err(JournalError::Invalid(
                "composer preparation has no sealed inputs",
            ));
        }
        Ok(())
    }

    fn validate_enqueue_preparation_receipt(
        &self,
        operation: &QueueOperation,
        receipt: &QueueJson,
    ) -> Result<(), JournalError> {
        if operation.preparation.is_none() {
            return Ok(());
        }
        let body = receipt.value()?;
        let contexts = body
            .get("receipt")
            .and_then(|receipt| receipt.get("contextIds"))
            .and_then(serde_json::Value::as_array)
            .ok_or(JournalError::Invalid(
                "prepared enqueue receipt omits its context identities",
            ))?;
        if contexts.iter().map(serde_json::Value::as_str).ne(self
            .source_context_ids(operation)?
            .iter()
            .map(|id| Some(id.as_str())))
        {
            return Err(JournalError::Invalid(
                "prepared enqueue receipt changed its complete context order",
            ));
        }
        Ok(())
    }

    fn validate_composer_preparation(
        &self,
        operation: &QueueOperation,
        attachments: &AttachmentCatalog,
        preparations: &[PreparationHold],
        retired: &[BlobId],
    ) -> Result<(), JournalError> {
        let Some(preparation) = &operation.preparation else {
            return Ok(());
        };
        let seal = operation.source_seal.as_ref().ok_or(JournalError::Invalid(
            "preparation has no original source seal",
        ))?;
        let owner = operation.owner.draft_key()?;
        if operation.source_intent.is_none()
            || preparation.owner()? != owner
            || preparation.source_sha256 != seal.sha256
            || preparation.generation == 0
            || preparation.generation == u64::MAX
            || preparation.revision == 0
            || preparation.revision == u64::MAX
            || preparation.sources.len() > MAX_ENTRIES
            || preparation.files.len() > MAX_ENTRIES
        {
            return Err(JournalError::Invalid(
                "composer preparation changed its captured source or owner",
            ));
        }
        validate_context_ids(&preparation.derived_context_ids)?;
        if preparation.sources.is_empty() {
            if !(capturing_composer(operation)
                || matches!(
                    operation.action(),
                    QueueAction::Send {
                        phase: QueueSendPhase::Deleted,
                        ..
                    }
                ))
                || !preparation.files.is_empty()
                || !preparation.derived_context_ids.is_empty()
                || preparation.inputs_object.is_some()
            {
                return Err(JournalError::Invalid(
                    "empty composer marker grants no prepared input authority",
                ));
            }
            return Ok(());
        }
        let source_count = 1 + operation
            .dependencies()
            .iter()
            .map(|reference| {
                self.local_take(&reference.operation)
                    .map_or(0, |local| local.transfer.items.len())
            })
            .sum::<usize>();
        let mut sources = HashSet::new();
        for source in &preparation.sources {
            let permitted = match source.identity {
                QueuePreparationSourceId::Source(index) => {
                    index < source_count && (index == 0 || operation.composer_edit().is_some())
                }
                QueuePreparationSourceId::Literal(piece) => {
                    piece < MAX_ENTRIES && operation.composer_edit().is_some()
                }
            };
            if !permitted
                || source.byte_len == 0
                || source.byte_len > crate::queue_capture::QUEUE_CAPTURE_BYTES
                || !sources.insert(source.identity)
            {
                return Err(JournalError::Invalid(
                    "preparation has no explicit captured input provenance",
                ));
            }
        }
        let mut labels = HashSet::new();
        let mut raw_sources = HashMap::new();
        let mut output_counts = HashMap::new();
        let mut groups = HashSet::new();
        for file in &preparation.files {
            let descriptor = file.descriptor()?;
            let hold = file.hold(&owner)?;
            let mention_units = file.mention.text.encode_utf16().count() as u64;
            if !sources.contains(&file.source)
                || file.preparation_generation != preparation.generation
                || !labels.insert(file.local_attachment_id.as_str())
                || file.output_ordinal >= MAX_ENTRIES
                || file.mention.start >= file.mention.end
                || file.mention.end > 9_007_199_254_740_991
                || file.mention.end - file.mention.start != mention_units
                || !file.mention.text.starts_with('@')
                || file.mention.text.len() > crate::mutation_journal::MAX_TEXT
                || file.raw_source_byte_len
                    > crate::attachment_files::MAX_INLINE_ATTACHMENT_BYTES as u64
                || crate::attachment_catalog::remote_path(&file.server_path).is_none()
                || openchamber_native_transport::attachment::AttachmentUpload::new(
                    &file.name,
                    Vec::new(),
                )
                .is_err()
                || file.mime_type.is_empty()
                || file.mime_type.len() > 255
                || !file.mime_type.contains('/')
                || file
                    .mime_type
                    .chars()
                    .any(|character| character.is_control() || character.is_whitespace())
            {
                return Err(JournalError::Invalid(
                    "invalid derived file source, mention or output identity",
                ));
            }
            let raw_identity = (
                file.mention.text.as_str(),
                file.server_path.as_str(),
                file.raw_source_byte_len,
                file.raw_source_sha256,
                file.document_group.as_deref(),
            );
            let bundle = (file.source, file.mention.start, file.mention.end);
            if raw_sources
                .insert(bundle, raw_identity)
                .is_some_and(|previous| previous != raw_identity)
            {
                return Err(JournalError::Invalid(
                    "derived mention bundle changed its raw source bytes",
                ));
            }
            let next_ordinal = output_counts.entry(bundle).or_insert(0usize);
            if file.output_ordinal != *next_ordinal {
                return Err(JournalError::Invalid(
                    "derived mention bundle changed its output order",
                ));
            }
            *next_ordinal += 1;
            if let Some(group) = &file.document_group {
                identifier(group)?;
                groups.insert(group.as_str());
            }
            if retired.contains(descriptor.id()) || self.contains_blob(descriptor.id()) {
                return Err(JournalError::Invalid(
                    "derived file aliases a retired or queue byte owner",
                ));
            }
            for other in &self.operations {
                let original = other
                    .native_roots
                    .iter()
                    .any(|root| root == &file.local_attachment_id);
                let transferred = resource_items_of(&other.action)
                    .chain(&other.prefix_downloads)
                    .any(|item| {
                        item.files
                            .iter()
                            .any(|binding| binding.local() == file.local_attachment_id)
                    });
                let original_transfer = items_of(&other.action)
                    .iter()
                    .chain(&other.prefix_downloads)
                    .any(|item| {
                        item.files
                            .iter()
                            .any(|binding| binding.local() == file.local_attachment_id)
                    });
                let derived = other.preparation.as_ref().is_some_and(|other| {
                    other.files.iter().any(|other| {
                        other.local_attachment_id == file.local_attachment_id
                            || other.document_group.as_deref()
                                == Some(file.local_attachment_id.as_str())
                    })
                });
                if original
                    || original_transfer
                    || (other.id != operation.id && (transferred || derived))
                {
                    return Err(JournalError::Invalid(
                        "derived file reuses an original or another operation's identity",
                    ));
                }
            }
            match file.state {
                QueueDerivedFileState::Proposed => {
                    if !preparations.contains(&hold)
                        || preparations
                            .iter()
                            .filter(|saved| saved.id == hold.id)
                            .count()
                            != 1
                        || attachments.contains_key(&hold.id)
                        || preparation.inputs_object.is_some()
                    {
                        return Err(JournalError::Invalid(
                            "derived proposal lost its exact pre-put hold",
                        ));
                    }
                }
                QueueDerivedFileState::Cataloged => {
                    if preparations.iter().any(|saved| saved.id == hold.id)
                        || attachments.get(&hold.id).is_none_or(|attachment| {
                            attachment.blob != descriptor
                                || attachment
                                    .staging
                                    .as_ref()
                                    .is_some_and(|staging| staging.filename != file.name)
                        })
                    {
                        return Err(JournalError::Invalid(
                            "derived file lost its exact catalog receipt",
                        ));
                    }
                    crate::attachment_catalog::validate_resources(
                        &mut 0,
                        attachments,
                        &owner,
                        &[hold.id],
                    )?;
                }
            }
        }
        if groups.iter().any(|group| labels.contains(group)) {
            return Err(JournalError::Invalid(
                "derived document group aliases output bytes",
            ));
        }
        for group in groups {
            if attachments.contains_key(group)
                || preparations.iter().any(|hold| hold.id.as_str() == group)
                || retired.iter().any(|id| id.as_str() == group)
                || self.operations.iter().any(|other| {
                    other.native_roots.iter().any(|root| root == group)
                        || (other.id != operation.id
                            && other.preparation.as_ref().is_some_and(|other| {
                                other.files.iter().any(|file| {
                                    file.local_attachment_id == group
                                        || file.document_group.as_deref() == Some(group)
                                })
                            }))
                })
            {
                return Err(JournalError::Invalid(
                    "derived document group aliases another source owner",
                ));
            }
        }
        if let Some(id) = &preparation.inputs_object {
            let object =
                self.require_object(operation, id, QueueObjectKind::NativeCompanion, true)?;
            if object.generation != preparation.generation
                || matches!(operation.source(), Some(QueueSource::Companion(source)) if source == id)
                || operation
                    .composer_edit()
                    .is_some_and(|edit| &edit.object == id)
            {
                return Err(JournalError::Invalid(
                    "prepared inputs changed their original object binding",
                ));
            }
        } else if !capturing_composer(operation)
            && !matches!(
                operation.action(),
                QueueAction::Send {
                    phase: QueueSendPhase::Deleted,
                    ..
                }
            )
        {
            return Err(JournalError::Invalid(
                "incomplete preparation escaped its capturing operation",
            ));
        }
        Ok(())
    }

    fn validate_source_seal(&self, operation: &QueueOperation) -> Result<(), JournalError> {
        let Some(seal) = &operation.source_seal else {
            return Err(JournalError::Invalid(
                "compound queue source has no original byte seal",
            ));
        };
        if seal.byte_len == 0 || seal.byte_len > crate::queue_capture::QUEUE_CAPTURE_BYTES {
            return Err(JournalError::Invalid("invalid compound source size"));
        }
        match operation.source() {
            Some(QueueSource::Inline(_)) => {
                let bytes = operation.compound_source_bytes()?;
                if bytes.len() as u64 != seal.byte_len || sha256(&bytes) != seal.sha256 {
                    return Err(JournalError::Invalid(
                        "compound source metadata changed its original capture",
                    ));
                }
            }
            Some(QueueSource::Companion(id)) => {
                let object =
                    self.require_object(operation, id, QueueObjectKind::NativeCompanion, true)?;
                if object.reference.total_bytes() != seal.byte_len
                    || object.reference.sha256() != &seal.sha256
                {
                    return Err(JournalError::Invalid(
                        "compound source pointer changed its original bytes",
                    ));
                }
            }
            None => {
                return Err(JournalError::Invalid(
                    "compound source has no original native content",
                ));
            }
        }
        Ok(())
    }

    fn validate_source_metadata(
        &self,
        operation: &QueueOperation,
        attachments: &AttachmentCatalog,
    ) -> Result<(), JournalError> {
        match (&operation.action, &operation.source_intent) {
            (QueueAction::Enqueue { .. }, Some(QueueSourceIntent::Enqueue(intent))) => {
                if intent.directory != operation.owner.directory {
                    return Err(JournalError::Invalid(
                        "compound enqueue changed its original directory",
                    ));
                }
                QueueJson::new(
                    serde_json::to_vec(intent)
                        .map_err(|_| JournalError::Invalid("invalid compound enqueue metadata"))?,
                )?;
            }
            (
                QueueAction::Prefix {
                    message,
                    request,
                    phase,
                    ..
                },
                Some(QueueSourceIntent::Prefix(intent)),
            ) => {
                validate_prefix_intent(intent, &operation.owner, message)?;
                if request.value()?.get("kind").is_some() {
                    let captured: NativePrefixIntent = serde_json::from_slice(request.bytes())
                        .map_err(|_| JournalError::Invalid("invalid internal prefix intent"))?;
                    if !matches!(phase, QueuePrefixPhase::Capturing) || &captured != intent {
                        return Err(JournalError::Invalid(
                            "internal prefix intent escaped its capturing phase",
                        ));
                    }
                } else {
                    let body = validate_real_prefix_request(request, &operation.owner, message)?;
                    if body.item_ids != intent.item_ids || body.delivery != intent.delivery {
                        return Err(JournalError::Invalid(
                            "real prefix request changed its original capture",
                        ));
                    }
                }
            }
            (
                QueueAction::Send {
                    message, delivery, ..
                },
                Some(QueueSourceIntent::Send(intent)),
            ) => {
                if intent.directory != operation.owner.directory
                    || intent.id != *message
                    || intent.delivery != *delivery
                {
                    return Err(JournalError::Invalid(
                        "local send changed its original capture owner or delivery",
                    ));
                }
                QueueJson::new(
                    serde_json::to_vec(intent)
                        .map_err(|_| JournalError::Invalid("invalid local send metadata"))?,
                )?;
            }
            (
                QueueAction::Prefix {
                    message, request, ..
                },
                None,
            ) => {
                validate_real_prefix_request(request, &operation.owner, message)?;
            }
            (QueueAction::Enqueue { .. } | QueueAction::Take { .. }, None) => {}
            _ => {
                return Err(JournalError::Invalid(
                    "source metadata differs from its actual operation kind",
                ));
            }
        }
        let Some(intent) = &operation.source_intent else {
            if operation.source_seal.is_some() {
                return Err(JournalError::Invalid(
                    "untyped operation has a compound source seal",
                ));
            }
            return Ok(());
        };
        self.validate_source_seal(operation)?;
        if let Some(edit) = intent.edit() {
            if edit.companions.as_slice() != operation.dependencies() {
                return Err(JournalError::Invalid(
                    "compound send changed its editing snapshot sources",
                ));
            }
            self.validate_composer_edit(
                &operation.owner.draft_key()?,
                edit,
                !self.retains_pending_consumer_edit(operation),
            )?;
        }
        if intent
            .files()
            .iter()
            .map(|file| file.id.as_str())
            .ne(operation.prepared_roots.iter().map(String::as_str))
        {
            return Err(JournalError::Invalid(
                "captured native file order changed its prepared roots",
            ));
        }
        for file in intent.files() {
            identifier(&file.id)?;
            let prepared = attachments
                .get(file.id.as_str())
                .ok_or(JournalError::Invalid(
                    "captured native file lost its original byte owner",
                ))?;
            if file.byte_len != prepared.blob.byte_len() || &file.sha256 != prepared.blob.sha256() {
                return Err(JournalError::Invalid(
                    "captured native file descriptor changed",
                ));
            }
        }
        if let Some(QueueSource::Inline(source)) = operation.source() {
            validate_native_capture(source, intent.native_context_ids(), intent.files())?;
        }
        validate_context_ids(intent.native_context_ids())?;
        Ok(())
    }

    fn validate_transfer(
        &self,
        operation: &QueueOperation,
        transfer: &QueueTransfer,
        attachments: &AttachmentCatalog,
        complete: bool,
    ) -> Result<(), JournalError> {
        validate_receipt(&transfer.receipt, operation, "take")?;
        if transfer.items.is_empty() || transfer.items.len() > 20 {
            return Err(JournalError::Invalid("invalid take item count"));
        }
        let request = match &operation.action {
            QueueAction::Take { request, .. } => request.value()?,
            _ => return Err(JournalError::Invalid("transfer belongs to a non-take")),
        };
        let expected = request
            .get("itemIds")
            .and_then(serde_json::Value::as_array)
            .ok_or(JournalError::Invalid("take request has no item identities"))?;
        let receipt = transfer.receipt.value()?;
        if receipt.get("state").and_then(serde_json::Value::as_str) != Some("transferring")
            || receipt
                .get("receipt")
                .and_then(|receipt| receipt.get("itemIds"))
                .and_then(serde_json::Value::as_array)
                != Some(expected)
        {
            return Err(JournalError::Invalid(
                "take receipt changed its transfer state or identities",
            ));
        }
        if expected.len() != transfer.items.len()
            || expected
                .iter()
                .zip(&transfer.items)
                .any(|(id, item)| id.as_str() != Some(item.item.as_str()))
        {
            return Err(JournalError::Invalid(
                "take changed its ordered item identities",
            ));
        }
        for item in &transfer.items {
            self.validate_item(operation, item, attachments, complete)?;
        }
        Ok(())
    }

    fn validate_prefix(
        &self,
        operation: &QueueOperation,
        message: &str,
        request: &QueueJson,
        handoff: &QueuePrefixHandoff,
    ) -> Result<(), JournalError> {
        if !operation.prefix_downloads.is_empty() && operation.prefix_downloads != handoff.items {
            return Err(JournalError::Invalid(
                "prefix handoff changed its prepared downloads",
            ));
        }
        handoff.receipt.validate()?;
        let receipt: PrefixReceipt = serde_json::from_slice(handoff.receipt.bytes())
            .map_err(|_| JournalError::Invalid("invalid native prefix receipt"))?;
        let request = validate_real_prefix_request(request, &operation.owner, message)?;
        let expected = &request.item_ids;
        let composer =
            self.require_object(operation, &handoff.composer, QueueObjectKind::Payload, true)?;
        if handoff.items.is_empty()
            || handoff.items.len() > 20
            || handoff.context_ids.len() > MAX_ENTRIES
            || expected.len() != handoff.items.len()
            || receipt.items.len() != handoff.items.len()
            || receipt.directory != operation.owner.directory()
            || receipt.message != message
            || receipt.steer != request.delivery.is_some()
            || receipt.context_ids != handoff.context_ids
            || &receipt.composer_sha256 != composer.reference.sha256()
        {
            return Err(JournalError::Invalid(
                "prefix receipt differs from captured request",
            ));
        }
        let pointer = request.pointer()?;
        if pointer.sha256() != receipt.composer_sha256
            || pointer.total_bytes() != composer.reference.total_bytes()
        {
            return Err(JournalError::Invalid(
                "prefix composer differs from retained bytes",
            ));
        }
        let mut contexts = Vec::new();
        for ((item, sealed), expected) in handoff.items.iter().zip(&receipt.items).zip(expected) {
            let index = QueueIndexedPayload::from_body(
                item.index.bytes(),
                operation.owner.directory(),
                sha256,
            )
            .map_err(|_| JournalError::Invalid("invalid prefix item index"))?;
            if expected != &item.item
                || sealed.item != item.item
                || sealed.payload_sha256 != index.pointer().sha256()
                || sealed.index_sha256 != index.index_hash()
            {
                return Err(JournalError::Invalid(
                    "prefix item differs from frozen preview",
                ));
            }
            contexts.extend(index.index().context_ids.iter().map(ToString::to_string));
        }
        if let Some(item) = &handoff.composer_item {
            let indexed = QueueIndexedPayload::from_body(
                item.index.bytes(),
                operation.owner.directory(),
                sha256,
            )
            .map_err(|_| JournalError::Invalid("invalid retained prefix composer index"))?;
            if item.payload != handoff.composer
                || indexed.pointer() != &pointer
                || indexed.index_hash() != receipt.composer_index_sha256
                || indexed.index().send_config != operation.configuration.restore()
                || (operation.source_intent.is_some()
                    && indexed
                        .index()
                        .context_ids
                        .iter()
                        .map(ToString::to_string)
                        .ne(self.source_context_ids(operation)?))
            {
                return Err(JournalError::Invalid(
                    "prefix composer changed its original configuration or context capture",
                ));
            }
            contexts.extend(indexed.index().context_ids.iter().map(ToString::to_string));
            if contexts != handoff.context_ids {
                return Err(JournalError::Invalid(
                    "prefix context order differs from its original sealed inputs",
                ));
            }
        } else if operation.source_intent.is_some() {
            return Err(JournalError::Invalid(
                "native prefix omits its original composer index and byte bindings",
            ));
        }
        Ok(())
    }

    fn validate_item(
        &self,
        operation: &QueueOperation,
        item: &QueueTransferItem,
        attachments: &AttachmentCatalog,
        complete: bool,
    ) -> Result<(), JournalError> {
        identifier(&item.item)?;
        let index =
            QueueIndexedPayload::from_body(item.index.bytes(), operation.owner.directory(), sha256)
                .map_err(|_| JournalError::Invalid("invalid compact queue payload index"))?;
        let object =
            self.require_object(operation, &item.payload, QueueObjectKind::Payload, complete)?;
        if object.reference.total_bytes() != index.pointer().total_bytes()
            || object.reference.sha256() != &index.pointer().sha256()
        {
            return Err(JournalError::Invalid(
                "local queue capture differs from indexed bytes",
            ));
        }
        let mut roots = HashSet::new();
        let mut ordinals = HashSet::new();
        let mut locals = HashSet::new();
        for file in &item.files {
            identifier(file.local())?;
            let descriptor = file.descriptor()?;
            if !locals.insert(file.local())
                || attachments
                    .get(file.local())
                    .is_some_and(|attachment| attachment.blob != descriptor)
            {
                return Err(JournalError::Invalid(
                    "queue attachment binding differs from local bytes",
                ));
            }
            if complete && !attachments.contains_key(file.local()) {
                return Err(JournalError::Invalid(
                    "take file lacks durable catalog ownership",
                ));
            }
            match file {
                QueueFileBinding::Manifest { blob, .. }
                    if index.index().attachment_blob_ids.contains(blob) && roots.insert(*blob) => {}
                QueueFileBinding::Legacy { ordinal, .. }
                    if index
                        .index()
                        .legacy_attachment_ids
                        .as_ref()
                        .is_some_and(|ids| *ordinal < ids.len())
                        && ordinals.insert(*ordinal) => {}
                _ => {
                    return Err(JournalError::Invalid(
                        "invalid or duplicate queue attachment binding",
                    ));
                }
            }
        }
        if complete
            && (roots.len() != index.index().attachment_blob_ids.len()
                || ordinals.len()
                    != index
                        .index()
                        .legacy_attachment_ids
                        .as_ref()
                        .map_or(0, Vec::len))
        {
            return Err(JournalError::Invalid(
                "take destination omits retained attachment bytes",
            ));
        }
        if complete {
            let resources: Vec<_> = item
                .files
                .iter()
                .map(|file| AttachmentId::from(file.local()))
                .collect();
            crate::attachment_catalog::validate_resources(
                &mut 0,
                attachments,
                &operation.owner.draft_key()?,
                &resources,
            )?;
        }
        Ok(())
    }

    fn validate_destination(
        &self,
        operation: &QueueOperation,
        purpose: &QueueTakePurpose,
        destination: &QueueDestination,
    ) -> Result<(), JournalError> {
        let companion = match (purpose, destination) {
            (QueueTakePurpose::Edit, QueueDestination::Draft { companion }) => companion,
            (
                QueueTakePurpose::Send {
                    message: expected, ..
                },
                QueueDestination::Send {
                    companion,
                    prompt,
                    message,
                },
            ) if expected == message => {
                self.require_object(operation, prompt, QueueObjectKind::FrozenSend, true)?;
                companion
            }
            _ => {
                return Err(JournalError::Invalid(
                    "take destination changed its captured purpose",
                ));
            }
        };
        self.require_object(operation, companion, QueueObjectKind::NativeCompanion, true)?;
        Ok(())
    }
}

fn capturing_composer(operation: &QueueOperation) -> bool {
    matches!(
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
    )
}

fn transfer_of(action: &QueueAction) -> Option<&QueueTransfer> {
    match action {
        QueueAction::Take {
            phase: QueueTakePhase::Transferring(transfer),
            ..
        } => Some(transfer),
        QueueAction::Take {
            phase:
                QueueTakePhase::LocalOwned(local)
                | QueueTakePhase::AcknowledgementUnknown(local)
                | QueueTakePhase::Acknowledged { local, .. },
            ..
        } => Some(&local.transfer),
        _ => None,
    }
}

fn items_of(action: &QueueAction) -> &[QueueTransferItem] {
    match action {
        QueueAction::Take { .. } => transfer_of(action).map_or(&[], |transfer| &transfer.items),
        QueueAction::Prefix {
            phase:
                QueuePrefixPhase::Previewed(handoff)
                | QueuePrefixPhase::ReservationUnknown(handoff)
                | QueuePrefixPhase::Reserved(handoff)
                | QueuePrefixPhase::Admitted(handoff),
            ..
        } => &handoff.items,
        _ => &[],
    }
}

fn resource_items_of(action: &QueueAction) -> impl Iterator<Item = &QueueTransferItem> {
    let composer = match action {
        QueueAction::Prefix {
            phase:
                QueuePrefixPhase::Previewed(handoff)
                | QueuePrefixPhase::ReservationUnknown(handoff)
                | QueuePrefixPhase::Reserved(handoff)
                | QueuePrefixPhase::Admitted(handoff),
            ..
        } => handoff.composer_item.as_ref(),
        _ => None,
    };
    items_of(action).iter().chain(composer)
}

fn ordered_references(owned: &[DraftCompanionRef], captured: &[DraftCompanionRef]) -> bool {
    let mut next = 0;
    for reference in owned {
        if captured.get(next) == Some(reference) {
            next += 1;
        }
    }
    next == captured.len()
        && captured
            .iter()
            .map(|reference| &reference.operation)
            .collect::<HashSet<_>>()
            .len()
            == captured.len()
}

fn validate_context_ids(ids: &[String]) -> Result<(), JournalError> {
    let mut unique = HashSet::new();
    if ids.len() > MAX_ENTRIES
        || ids
            .iter()
            .any(|id| identifier(id).is_err() || !id.starts_with("msg_") || !unique.insert(id))
    {
        return Err(JournalError::Invalid(
            "invalid native context admission identities",
        ));
    }
    Ok(())
}

fn validate_native_capture(
    source: &QueueNativeSource,
    contexts: &[String],
    files: &[QueueNativeFileDescriptor],
) -> Result<(), JournalError> {
    crate::draft_storage::validate(&mut 0, &source.draft)?;
    validate_context_ids(contexts)?;
    let count: usize = source
        .draft
        .content
        .context
        .iter()
        .map(|context| {
            usize::from(context.instructions.is_some())
                + usize::from(!context.text.trim().is_empty())
        })
        .sum();
    if contexts.len() != count
        || files.len() != source.draft.content.files.len()
        || files
            .iter()
            .zip(&source.draft.content.files)
            .any(|(descriptor, file)| {
                descriptor.id != file.id.as_str()
                    || !matches!(file.content, DraftFileContent::Prepared)
                    || file.byte_len != Some(descriptor.byte_len)
                    || descriptor.byte_len
                        > openchamber_native_transport::attachment::MAX_PREPARED_ATTACHMENT_BYTES
                            as u64
            })
    {
        return Err(JournalError::Invalid(
            "native capture context IDs or prepared files differ from its original draft",
        ));
    }
    Ok(())
}

fn validate_prefix_intent(
    intent: &NativePrefixIntent,
    owner: &QueueOwner,
    message: &str,
) -> Result<(), JournalError> {
    if intent.directory != owner.directory
        || intent.id != message
        || intent.delivery == Some(Delivery::Queue)
    {
        return Err(JournalError::Invalid(
            "native prefix intent changed its owner, message or delivery",
        ));
    }
    validate_item_ids(&intent.item_ids)?;
    validate_context_ids(&intent.native_context_ids)?;
    QueueJson::new(
        serde_json::to_vec(intent)
            .map_err(|_| JournalError::Invalid("invalid prefix intent metadata"))?,
    )?;
    Ok(())
}

fn validate_item_ids(ids: &[String]) -> Result<(), JournalError> {
    let mut unique = HashSet::new();
    if ids.is_empty()
        || ids.len() > 20
        || ids
            .iter()
            .any(|id| identifier(id).is_err() || !unique.insert(id))
    {
        return Err(JournalError::Invalid(
            "invalid ordered prefix item identities",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PrefixRequest {
    format: String,
    directory: String,
    id: String,
    item_ids: Vec<String>,
    composer: UniqueJson,
    #[serde(default)]
    delivery: Option<Delivery>,
}

impl PrefixRequest {
    fn pointer(&self) -> Result<QueuePayloadPointer, JournalError> {
        QueuePayloadPointer::from_body(
            &serde_json::to_vec(&self.composer.0)
                .map_err(|_| JournalError::Invalid("prefix composer pointer cannot be encoded"))?,
        )
        .map_err(|_| JournalError::Invalid("invalid strict prefix composer pointer"))
    }
}

fn validate_real_prefix_request(
    request: &QueueJson,
    owner: &QueueOwner,
    message: &str,
) -> Result<PrefixRequest, JournalError> {
    request.validate()?;
    let body: PrefixRequest = serde_json::from_slice(request.bytes())
        .map_err(|_| JournalError::Invalid("invalid strict retained payload prefix request"))?;
    if body.format != "payload-v1"
        || body.directory != owner.directory
        || body.id != message
        || body.delivery == Some(Delivery::Queue)
    {
        return Err(JournalError::Invalid(
            "prefix request changed its exact capture owner or delivery",
        ));
    }
    validate_item_ids(&body.item_ids)?;
    body.pointer()?;
    Ok(body)
}

/// Preserve the transport parser's duplicate-field refusal inside the pointer.
struct UniqueJson(serde_json::Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueJson;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON without duplicate object fields")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut fields = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, UniqueJson>()? {
                    if fields.insert(key, value.0).is_some() {
                        return Err(serde::de::Error::custom("duplicate retained pointer field"));
                    }
                }
                Ok(UniqueJson(serde_json::Value::Object(fields)))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut items = Vec::new();
                while let Some(value) = sequence.next_element::<UniqueJson>()? {
                    items.push(value.0);
                }
                Ok(UniqueJson(serde_json::Value::Array(items)))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|value| UniqueJson(value.into()))
                    .ok_or_else(|| serde::de::Error::custom("invalid retained JSON number"))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(serde_json::Value::Null))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

fn reserve_message<'a>(
    messages: &mut HashSet<(&'a str, &'a str)>,
    operation: &'a QueueOperation,
    message: &'a str,
) -> Result<(), JournalError> {
    identifier(message)?;
    if !message.starts_with("msg_") || !messages.insert((operation.owner.runtime(), message)) {
        return Err(JournalError::Invalid("duplicate queue admission identity"));
    }
    Ok(())
}

fn validate_directory_body(body: &QueueJson, owner: &QueueOwner) -> Result<(), JournalError> {
    if body
        .value()?
        .get("directory")
        .and_then(serde_json::Value::as_str)
        != Some(owner.directory())
    {
        return Err(JournalError::Invalid(
            "retained queue body changed its exact directory",
        ));
    }
    Ok(())
}

fn validate_enqueue_retry(
    retry: &QueueJson,
    receipt: &QueueJson,
    owner: &QueueOwner,
) -> Result<(), JournalError> {
    validate_directory_body(retry, owner)?;
    let saved = receipt.value()?;
    let body = retry.value()?;
    let previous = saved
        .get("attempt")
        .and_then(serde_json::Value::as_u64)
        .ok_or(JournalError::Invalid(
            "enqueue retry lacks its previous attempt",
        ))?;
    if saved.get("state").and_then(serde_json::Value::as_str) != Some("uncertain")
        || body.get("attempt").and_then(serde_json::Value::as_u64) != previous.checked_add(1)
        || previous >= 9_007_199_254_740_991
        || body.as_object().is_none_or(|fields| fields.len() != 2)
    {
        return Err(JournalError::Invalid(
            "invalid explicit enqueue retry authorization",
        ));
    }
    Ok(())
}

fn validate_receipt(
    body: &QueueJson,
    operation: &QueueOperation,
    kind: &str,
) -> Result<(), JournalError> {
    let value = body.value()?;
    let receipt = value
        .get("receipt")
        .ok_or(JournalError::Invalid("queue reply lacks its receipt"))?;
    let field = |key| receipt.get(key).and_then(serde_json::Value::as_str);
    if field("kind") != Some(kind)
        || field("requestId") != Some(operation.id.as_str())
        || field("sessionId") != Some(operation.owner.session())
        || field("directory") != Some(operation.owner.directory())
        || receipt.get("version").and_then(serde_json::Value::as_u64) != Some(1)
        || field("requestHash").is_none_or(|hash| {
            hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        || receipt
            .get("acceptedAt")
            .and_then(serde_json::Value::as_u64)
            .is_none_or(|time| time > 9_007_199_254_740_991)
    {
        return Err(JournalError::Invalid(
            "queue receipt differs from retained request owner",
        ));
    }
    Ok(())
}

fn hex_digest(digest: &[u8; 32]) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing a digest to a String cannot fail");
    }
    encoded
}

#[cfg(all(test, unix))]
#[path = "queue_storage_tests.rs"]
pub(crate) mod tests;
