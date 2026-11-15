//! Queue-owned OpenCode 2.2.5 sends, without the narrower composer representation.
//!
//! Capture is pure and must finish before prefix reservation. The caller retains
//! the original payloads, prepared-file ownership and this capture in its spool;
//! only compact descriptors belong in the journal. Dispatch never reserves,
//! prepares files, answers requests, allocates IDs, or retries automatically.
//!
//! The spool frame is `OCQSEND1`, a big-endian u32 metadata length, that strict
//! JSON metadata, each synthetic JSON body in order, then the prompt JSON body.
//! Metadata is at most 4 MiB and the whole frame is at most 128 MiB. Raw bodies
//! avoid double JSON escaping and remain byte-identical after reconstruction.
//! Strict queue inputs require a sealed materialization. Ordinary sends may mix
//! those with explicit local captures, which carry no queue transfer authority.
//! Each source is at most 50 MiB and their combined structured content is at most
//! 50 MiB. Borrowed plans
//! copy no attachment bytes; legacy verification decodes at most one 20 MiB file.
//! Capture counts every body before allocating the single outgoing frame.
//!
//! Display content and previews remain in the original payload spool. Only each
//! item's delivered `text` enters the prompt. Multiple nonblank delivered texts
//! join with two newlines, as queue prefix sends do; a single text stays exact.
//! The caller chooses the input whose original configuration controls the send.
//! All file, agent, skill and context occurrences retain their original order.
//! A repeated top-level metadata key must have the same JSON value. Different
//! objects under one key are a conflict, with no deep merge or overwrite.
//! New bodies use the pinned SDK's JSON.stringify number spelling. Reconstructed
//! frames retain their original body bytes, including earlier number spellings.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io;
use std::ops::Range;

use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::message_queue::QueuedSendConfig;
use openchamber_native_core::send_config::ModelChoice;
use openchamber_native_core::transcript::Role;
use reqwest::{Method, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::prompt::{
    Delivery, PromptAdmission, PromptObservation, PromptReceipt, SendError, SendObservation,
    SendStage,
};
use crate::prompt_parts::SyntheticBody;
use crate::queue_manifest::{
    QUEUE_ADMISSION_BYTES, QUEUE_BLOB_BYTES, QUEUE_MANIFEST_BYTES, QUEUE_MANIFEST_ENTRIES,
    QUEUE_PAYLOAD_BYTES, QueueIndexedPayload, QueueLegacyContext, QueueLocalCapture,
    QueueLocalItem, QueueMention, QueuePayloadDocument, QueuePayloadFileSource, QueuePayloadItem,
    QueueScope, QueueSha256, compact_queue_json_len, queue_file_uri, write_queue_request_json,
};
use crate::request::RequestBuilder;

const MAGIC: &[u8; 8] = b"OCQSEND1";
const HEADER_BYTES: usize = 12;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Content-free capture refusals. A refusal grants no queue reservation authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum QueueFrozenSendError {
    #[error("invalid frozen queue send")]
    InvalidInput,
    #[error("frozen queue send belongs to another owner or configuration")]
    OwnerMismatch,
    #[error("prepared queue file binding differs from its capture")]
    FileMismatch,
    #[error("queued prompt metadata conflicts")]
    MetadataConflict,
    #[error("frozen queue metadata exceeds 4 MiB")]
    MetadataTooLarge,
    #[error("combined queue content exceeds 50 MiB")]
    StructuredTooLarge,
    #[error("frozen queue send exceeds 128 MiB")]
    OutgoingTooLarge,
    #[error("frozen queue send digest differs")]
    DigestMismatch,
}

/// A caller-verified prepared file staged on the captured runtime.
///
/// Construction validates the URI and byte descriptor, not a remote write. The
/// caller must bind this URI to those exact durably held bytes, using its captured
/// attachment plan. Source paths in queue metadata must never supply this URI.
#[derive(Clone, PartialEq, Eq)]
pub struct QueuePreparedFile {
    source: PreparedFileSource,
    byte_len: u64,
    sha256: [u8; 32],
    uri: String,
}

#[derive(Clone, PartialEq, Eq)]
enum PreparedFileSource {
    Queued(QueuePayloadFileSource),
    Local(usize),
}

impl QueuePreparedFile {
    /// Bind one exact prepared-byte descriptor to the caller's validated staging
    /// URI. This performs no upload, path lookup or filesystem read.
    pub fn new(
        source: QueuePayloadFileSource,
        byte_len: u64,
        sha256: [u8; 32],
        uri: &str,
    ) -> Result<Self, QueueFrozenSendError> {
        Self::bind(PreparedFileSource::Queued(source), byte_len, sha256, uri)
    }

    /// Bind a local capture's file ordinal, without queue upload identities.
    pub fn new_local(
        ordinal: usize,
        byte_len: u64,
        sha256: [u8; 32],
        uri: &str,
    ) -> Result<Self, QueueFrozenSendError> {
        if ordinal >= QUEUE_MANIFEST_ENTRIES {
            return Err(QueueFrozenSendError::FileMismatch);
        }
        Self::bind(PreparedFileSource::Local(ordinal), byte_len, sha256, uri)
    }

    fn bind(
        source: PreparedFileSource,
        byte_len: u64,
        sha256: [u8; 32],
        uri: &str,
    ) -> Result<Self, QueueFrozenSendError> {
        if byte_len > QUEUE_BLOB_BYTES as u64 || !prepared_uri(uri) {
            return Err(QueueFrozenSendError::FileMismatch);
        }
        Ok(Self {
            source,
            byte_len,
            sha256,
            uri: uri.to_owned(),
        })
    }
}

impl fmt::Debug for QueuePreparedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuePreparedFile")
            .field("byte_len", &self.byte_len)
            .finish_non_exhaustive()
    }
}

/// One sealed, materialized occurrence in send order. The index supplies the
/// complete context ID sequence, including legacy instructions. Manifest blob
/// aliases share one prepared binding; legacy bindings use file array ordinals.
#[derive(Debug)]
pub struct QueueFrozenItem<'a> {
    pub document: &'a QueuePayloadDocument,
    pub indexed_payload: &'a QueueIndexedPayload,
    pub files: &'a [QueuePreparedFile],
}

/// A local edit/send capture and byte bindings. This type has no queue transfer
/// receipt or index. Ordinals bind only to this local capture's ordered files.
#[derive(Debug)]
pub struct QueueLocalFrozenItem<'a> {
    pub capture: &'a QueueLocalCapture,
    pub files: &'a [QueuePreparedFile],
}

/// Explicit source authority for an ordinary rich send. Sealed companions keep
/// their original queue authority; local fields never acquire it by merging.
#[derive(Debug)]
pub enum QueueFrozenSource<'a> {
    Queued(QueueFrozenItem<'a>),
    Local(QueueLocalFrozenItem<'a>),
}

/// Ordered ordinary-send inputs. An empty server queue needs only one Local
/// input; no preview, reservation, upload or fabricated queue item is required.
pub struct QueueLocalSendInput<'a> {
    pub session: &'a SessionId,
    pub directory: &'a str,
    pub message_id: &'a MessageId,
    pub sources: &'a [QueueFrozenSource<'a>],
    pub selected_input: usize,
    pub delivery: Option<Delivery>,
}

impl fmt::Debug for QueueLocalSendInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueLocalSendInput")
            .field("session", self.session)
            .field("message_id", self.message_id)
            .field("sources", &self.sources.len())
            .finish_non_exhaustive()
    }
}

/// Retained provenance only. A reconstructed frozen frame grants no file proof
/// or acknowledgement capability, including when it originated from a queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueFrozenSourceKind {
    Queued,
    Local,
    LegacyFrame,
}

/// One request from a frozen send. The caller can check foreground admission
/// between requests without reconstructing or changing the captured bodies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueueFrozenDispatchStep {
    /// Select the captured model and effort.
    Model,
    /// Select the captured agent, which must be present.
    Agent,
    /// Admit the next zero-based synthetic context item.
    Context(usize),
    /// Admit the original prompt after all context items are admitted.
    Prompt,
}

/// Captured identities and an explicit original configuration selection.
/// Prefix callers put the composer last and select that input. A take/send may
/// select the first input. No mutable composer selection is resolved here.
pub struct QueueFrozenSendInput<'a> {
    pub session: &'a SessionId,
    pub directory: &'a str,
    pub message_id: &'a MessageId,
    pub items: &'a [QueueFrozenItem<'a>],
    pub selected_input: usize,
    pub delivery: Option<Delivery>,
}

impl fmt::Debug for QueueFrozenSendInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueFrozenSendInput")
            .field("session", self.session)
            .field("message_id", self.message_id)
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}

/// Exact spool bytes plus compact offsets and a captured authenticated client.
/// The byte buffer is never serialized into a journal or cloned by this type.
pub struct QueueFrozenSend {
    client: ServerClient,
    scope: QueueScope,
    metadata: FrameMetadata,
    bytes: Vec<u8>,
    digest: [u8; 32],
    context_ranges: Vec<Range<usize>>,
    prompt_range: Range<usize>,
}

impl fmt::Debug for QueueFrozenSend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueFrozenSend")
            .field("session", self.scope.session())
            .field("message_id", &self.metadata.message_id)
            .field("contexts", &self.context_ranges.len())
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Configuration {
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(rename = "modelID")]
    model_id: String,
    agent: Option<String>,
    variant: Option<String>,
}

impl Configuration {
    fn capture(config: &QueuedSendConfig) -> Self {
        Self {
            provider_id: config.provider_id.clone(),
            model_id: config.model_id.clone(),
            agent: config.agent.clone(),
            variant: config.variant.clone(),
        }
    }

    fn restore(&self) -> QueuedSendConfig {
        QueuedSendConfig {
            provider_id: self.provider_id.clone(),
            model_id: self.model_id.clone(),
            agent: self.agent.clone(),
            variant: self.variant.clone(),
        }
    }

    fn valid(&self) -> bool {
        [&self.provider_id, &self.model_id]
            .into_iter()
            .chain(self.agent.iter())
            .chain(self.variant.iter())
            .all(|value| identifier(value))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum SourceMetadata {
    Current(CurrentSourceMetadata),
    Legacy(LegacySourceMetadata),
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CurrentSourceMetadata {
    capture_sha256: [u8; 32],
    provenance: SourceProvenance,
    structured_bytes: u64,
    configuration: Configuration,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum SourceProvenance {
    Queued {
        #[serde(rename = "indexSha256")]
        index_sha256: [u8; 32],
    },
    Local {},
}

// Earlier OCQSEND1 frames have no explicit provenance tag. Keep that original
// representation and classify it separately, without minting transfer evidence.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacySourceMetadata {
    capture_sha256: [u8; 32],
    index_sha256: [u8; 32],
    structured_bytes: u64,
    configuration: Configuration,
}

impl SourceMetadata {
    fn configuration(&self) -> &Configuration {
        match self {
            Self::Current(source) => &source.configuration,
            Self::Legacy(source) => &source.configuration,
        }
    }
    fn structured_bytes(&self) -> u64 {
        match self {
            Self::Current(source) => source.structured_bytes,
            Self::Legacy(source) => source.structured_bytes,
        }
    }
    fn kind(&self) -> QueueFrozenSourceKind {
        match self {
            Self::Current(CurrentSourceMetadata {
                provenance: SourceProvenance::Queued { .. },
                ..
            }) => QueueFrozenSourceKind::Queued,
            Self::Current(CurrentSourceMetadata {
                provenance: SourceProvenance::Local {},
                ..
            }) => QueueFrozenSourceKind::Local,
            Self::Legacy(_) => QueueFrozenSourceKind::LegacyFrame,
        }
    }
}

#[derive(Clone, Copy)]
enum SourceItem<'a> {
    Queued(&'a QueuePayloadItem),
    Local(&'a QueueLocalItem),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContextMetadata {
    id: String,
    bytes: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrameMetadata {
    endpoint: String,
    session: String,
    directory: String,
    message_id: String,
    selected_input: usize,
    sources: Vec<SourceMetadata>,
    delivery: Option<Delivery>,
    context: Vec<ContextMetadata>,
    prompt_bytes: usize,
    /// New captures identify explicit path references separately from prepared
    /// files. Absence preserves earlier frames' original file-name contract.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    file_reference_ordinals: Vec<usize>,
}

#[derive(Serialize)]
struct MentionBody<'a> {
    start: u64,
    end: u64,
    text: &'a str,
}

fn mention(value: Option<&QueueMention>) -> Option<MentionBody<'_>> {
    value.map(|value| MentionBody {
        start: value.start,
        end: value.end,
        text: &value.text,
    })
}

#[derive(Serialize)]
struct FileBody<'a> {
    uri: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mention: Option<MentionBody<'a>>,
}

#[derive(Serialize)]
struct AgentBody<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    mention: Option<MentionBody<'a>>,
}

#[derive(Serialize)]
struct SkillBody<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    mention: Option<MentionBody<'a>>,
}

#[derive(Serialize)]
struct PromptBody<'a> {
    id: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    files: &'a [FileBody<'a>],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    agents: &'a [AgentBody<'a>],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    skills: &'a [SkillBody<'a>],
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a BTreeMap<&'a str, &'a Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delivery: Option<Delivery>,
}

impl QueueFrozenSend {
    /// Encode before any reservation or admission. Inputs are borrowed only for
    /// this call. File verification uses the destination's SHA-256 implementation.
    /// Freshly parsed or edited captures must first receive and materialize their
    /// returned queue index; parsing alone supplies no frozen-send authority.
    pub fn capture(
        client: &ServerClient,
        input: QueueFrozenSendInput<'_>,
        hash: QueueSha256,
    ) -> Result<Self, QueueFrozenSendError> {
        if input.items.is_empty()
            || input.items.len() > 21
            || input.selected_input >= input.items.len()
        {
            return Err(QueueFrozenSendError::InvalidInput);
        }
        let sources: Vec<_> = input
            .items
            .iter()
            .map(|source| {
                QueueFrozenSource::Queued(QueueFrozenItem {
                    document: source.document,
                    indexed_payload: source.indexed_payload,
                    files: source.files,
                })
            })
            .collect();
        Self::capture_sources(
            client,
            QueueLocalSendInput {
                session: input.session,
                directory: input.directory,
                message_id: input.message_id,
                sources: &sources,
                selected_input: input.selected_input,
                delivery: input.delivery,
            },
            hash,
        )
    }

    /// Capture an ordinary rich send from explicitly local fields and/or sealed
    /// companions. Source kinds remain distinct; this creates no queue receipt,
    /// file proof, upload, reservation or acknowledgement authority.
    pub fn capture_local(
        client: &ServerClient,
        input: QueueLocalSendInput<'_>,
        hash: QueueSha256,
    ) -> Result<Self, QueueFrozenSendError> {
        Self::capture_sources(client, input, hash)
    }

    fn capture_sources(
        client: &ServerClient,
        input: QueueLocalSendInput<'_>,
        hash: QueueSha256,
    ) -> Result<Self, QueueFrozenSendError> {
        let scope = QueueScope::new(client, input.session.clone(), input.directory)
            .map_err(|_| QueueFrozenSendError::InvalidInput)?;
        if input.sources.is_empty()
            || input.sources.len() > 21
            || input.selected_input >= input.sources.len()
            || !message_id(input.message_id.as_str())
        {
            return Err(QueueFrozenSendError::InvalidInput);
        }
        let mut texts = Vec::new();
        let mut files = Vec::new();
        let mut file_reference_ordinals = Vec::new();
        let mut agents = Vec::new();
        let mut skills = Vec::new();
        let mut context = Vec::new();
        let mut metadata: BTreeMap<&str, &Value> = BTreeMap::new();
        let mut has_metadata = false;
        let mut ids = HashSet::from([input.message_id.as_str()]);
        let mut sources = Vec::new();
        let mut structured_bytes = 0u64;

        let mut context_ids: Vec<&MessageId> = Vec::new();
        for source in input.sources {
            let (item, source_ids, source_files, capture_sha256, provenance, known_size, proof) =
                match source {
                    QueueFrozenSource::Queued(source) => {
                        if source.document.directory() != input.directory {
                            return Err(QueueFrozenSendError::OwnerMismatch);
                        }
                        if source.document.bytes().len() > QUEUE_PAYLOAD_BYTES {
                            return Err(QueueFrozenSendError::StructuredTooLarge);
                        }
                        // Only real materialization grants this proof. Never construct
                        // a queue index or a file proof for a local capture.
                        let proof = source
                            .document
                            .files_proof(hash)
                            .map_err(|_| QueueFrozenSendError::FileMismatch)?;
                        let indexed = source.indexed_payload;
                        let index = indexed.index();
                        if proof.index_sha256() != &indexed.index_hash()
                            || proof.capture_sha256() != &indexed.pointer().sha256()
                            || indexed.pointer().sha256() != source.document.sha256()
                            || indexed.pointer().total_bytes()
                                != source.document.bytes().len() as u64
                            || index.directory != input.directory
                            || &index.send_config != source.document.item().send_config()
                        {
                            return Err(QueueFrozenSendError::OwnerMismatch);
                        }
                        (
                            SourceItem::Queued(source.document.item()),
                            index.context_ids.as_slice(),
                            source.files,
                            source.document.sha256(),
                            SourceProvenance::Queued {
                                index_sha256: indexed.index_hash(),
                            },
                            Some(index.structured_bytes),
                            Some(proof),
                        )
                    }
                    QueueFrozenSource::Local(source) => {
                        if source.capture.directory() != input.directory {
                            return Err(QueueFrozenSendError::OwnerMismatch);
                        }
                        if source.capture.bytes().len() > QUEUE_PAYLOAD_BYTES {
                            return Err(QueueFrozenSendError::StructuredTooLarge);
                        }
                        (
                            SourceItem::Local(source.capture.item()),
                            source.capture.context_ids(),
                            source.files,
                            source.capture.sha256(),
                            SourceProvenance::Local {},
                            None,
                            None,
                        )
                    }
                };
            let config = Configuration::capture(match &item {
                SourceItem::Queued(item) => item.send_config(),
                SourceItem::Local(item) => &item.send_config,
            });
            if !config.valid() {
                return Err(QueueFrozenSendError::InvalidInput);
            }
            let mut used_files = HashSet::new();
            if source_files.len() > QUEUE_MANIFEST_ENTRIES {
                return Err(QueueFrozenSendError::FileMismatch);
            }
            for binding in source_files {
                if binding.byte_len > QUEUE_BLOB_BYTES as u64 || !prepared_uri(&binding.uri) {
                    return Err(QueueFrozenSendError::FileMismatch);
                }
            }
            let (text, legacy, parts) = match item {
                SourceItem::Queued(QueuePayloadItem::Manifest(item)) => {
                    for file in &item.attachment_manifest {
                        let (ordinal, binding) = binding(
                            source_files,
                            &PreparedFileSource::Queued(QueuePayloadFileSource::Manifest(
                                file.attachment.blob_id,
                            )),
                        )?;
                        if binding.byte_len != file.attachment.size
                            || binding.sha256 != file.attachment.sha256
                        {
                            return Err(QueueFrozenSendError::FileMismatch);
                        }
                        used_files.insert(ordinal);
                        files.push(FileBody {
                            uri: &binding.uri,
                            name: file
                                .agent_path
                                .as_deref()
                                .unwrap_or(&file.attachment.filename),
                            description: file.description.as_deref(),
                            mention: mention(file.mention.as_ref()),
                        });
                    }
                    (
                        &item.text,
                        item.context.as_slice(),
                        Some(&item.prompt_parts),
                    )
                }
                SourceItem::Queued(QueuePayloadItem::Legacy(item)) => {
                    for (index, file) in item.attachments.iter().enumerate() {
                        let (ordinal, binding) = binding(
                            source_files,
                            &PreparedFileSource::Queued(QueuePayloadFileSource::Legacy(index)),
                        )?;
                        let original = proof
                            .as_ref()
                            .expect("queued source has its sealed proof")
                            .files()
                            .iter()
                            .find(|file| file.source() == &QueuePayloadFileSource::Legacy(index))
                            .ok_or(QueueFrozenSendError::FileMismatch)?;
                        if binding.byte_len != original.byte_len()
                            || &binding.sha256 != original.sha256()
                        {
                            return Err(QueueFrozenSendError::FileMismatch);
                        }
                        used_files.insert(ordinal);
                        files.push(FileBody {
                            uri: &binding.uri,
                            name: file.agent_path.as_deref().unwrap_or(&file.filename),
                            description: None,
                            mention: None,
                        });
                    }
                    for file in &item.file_references {
                        file_reference_ordinals.push(files.len());
                        files.push(FileBody {
                            uri: &file.uri,
                            name: &file.name,
                            description: file.description.as_deref(),
                            mention: mention(file.mention.as_ref()),
                        });
                    }
                    if let Some(name) = &item.agent_mention {
                        agents.push(AgentBody {
                            name,
                            mention: None,
                        });
                    }
                    (&item.text, item.context.as_slice(), None)
                }
                SourceItem::Local(item) => {
                    for (index, file) in item.files.iter().enumerate() {
                        let (ordinal, binding) =
                            binding(source_files, &PreparedFileSource::Local(index))?;
                        if binding.byte_len != file.byte_len || binding.sha256 != file.sha256 {
                            return Err(QueueFrozenSendError::FileMismatch);
                        }
                        used_files.insert(ordinal);
                        files.push(FileBody {
                            uri: &binding.uri,
                            name: &file.name,
                            description: file.description.as_deref(),
                            mention: mention(file.mention.as_ref()),
                        });
                    }
                    (&item.text, &[][..], Some(&item.prompt_parts))
                }
            };
            if let Some(parts) = parts {
                for file in &parts.file_references {
                    file_reference_ordinals.push(files.len());
                    files.push(FileBody {
                        uri: &file.uri,
                        name: &file.name,
                        description: file.description.as_deref(),
                        mention: mention(file.mention.as_ref()),
                    });
                }
                if let Some(values) = &parts.metadata {
                    has_metadata = true;
                    for (key, value) in &values.0 {
                        if metadata
                            .get(key.as_str())
                            .is_some_and(|old| !json_equal(old, value))
                        {
                            return Err(QueueFrozenSendError::MetadataConflict);
                        }
                        metadata.entry(key.as_str()).or_insert(value);
                    }
                }
                for agent in &parts.agents {
                    agents.push(AgentBody {
                        name: &agent.name,
                        mention: mention(agent.mention.as_ref()),
                    });
                }
                for skill in &parts.skills {
                    skills.push(SkillBody {
                        id: &skill.id,
                        mention: mention(skill.mention.as_ref()),
                    });
                }
            }
            if used_files.len() != source_files.len() {
                return Err(QueueFrozenSendError::FileMismatch);
            }
            texts.push(text.as_str());
            let begin = context.len();
            for part in legacy {
                match part {
                    QueueLegacyContext::Context {
                        text,
                        metadata,
                        instructions,
                    } => {
                        if let Some(instructions) = instructions {
                            context.push((instructions.as_str(), None, None, None));
                        }
                        context.push((text.as_str(), None, Some(metadata), None));
                    }
                    QueueLegacyContext::Instruction { text }
                    | QueueLegacyContext::Synthetic { text } => {
                        context.push((text.as_str(), None, None, None));
                    }
                }
            }
            for part in parts.map_or(&[][..], |parts| parts.context.as_slice()) {
                context.push((
                    part.text.as_str(),
                    part.description.as_deref(),
                    part.metadata.as_ref(),
                    part.id.as_ref(),
                ));
            }
            if context.len() - begin != source_ids.len() {
                return Err(QueueFrozenSendError::InvalidInput);
            }
            for ((_, _, _, explicit), id) in context[begin..].iter().zip(source_ids) {
                if !message_id(id.as_str())
                    || !ids.insert(id.as_str())
                    || explicit.is_some_and(|saved| saved != id)
                {
                    return Err(QueueFrozenSendError::InvalidInput);
                }
            }
            context_ids.extend(source_ids.iter());
            let source_size = match (known_size, &item) {
                (Some(size), _) => size,
                (None, SourceItem::Local(item)) => local_structured_bytes(item)?,
                _ => return Err(QueueFrozenSendError::InvalidInput),
            };
            structured_bytes = structured_bytes
                .checked_add(source_size)
                .filter(|size| *size <= QUEUE_PAYLOAD_BYTES as u64)
                .ok_or(QueueFrozenSendError::StructuredTooLarge)?;
            sources.push(SourceMetadata::Current(CurrentSourceMetadata {
                capture_sha256,
                provenance,
                structured_bytes: source_size,
                configuration: config,
            }));
        }
        if files.len() + agents.len() + skills.len() + context.len() > QUEUE_MANIFEST_ENTRIES {
            return Err(QueueFrozenSendError::InvalidInput);
        }
        let raw_text_bytes = texts.iter().map(|text| text.len()).sum::<usize>();
        if raw_text_bytes > QUEUE_PAYLOAD_BYTES {
            return Err(QueueFrozenSendError::StructuredTooLarge);
        }
        let text = if texts.len() == 1 {
            texts[0].to_owned()
        } else {
            texts
                .into_iter()
                .filter(|text| !js_trim(text).is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        if js_trim(&text).is_empty() && files.is_empty() && context.is_empty() {
            return Err(QueueFrozenSendError::InvalidInput);
        }
        let prompt = PromptBody {
            id: input.message_id.as_str(),
            text: &text,
            files: &files,
            agents: &agents,
            skills: &skills,
            metadata: has_metadata.then_some(&metadata),
            delivery: input.delivery,
        };
        let bodies: Vec<_> = context
            .iter()
            .zip(&context_ids)
            .map(|((text, description, metadata, _), id)| SyntheticBody {
                id: id.as_str(),
                text,
                description: *description,
                metadata: *metadata,
                delivery: input.delivery,
                resume: false,
            })
            .collect();
        // Count actual encoded bodies before allocating their combined frame.
        let context_metadata = bodies
            .iter()
            .map(|body| {
                Ok(ContextMetadata {
                    id: body.id.to_owned(),
                    bytes: encoded_len(body, QUEUE_ADMISSION_BYTES as usize)?,
                })
            })
            .collect::<Result<Vec<_>, QueueFrozenSendError>>()?;
        let frame = FrameMetadata {
            endpoint: client.endpoint().as_str().to_owned(),
            session: input.session.to_string(),
            directory: input.directory.to_owned(),
            message_id: input.message_id.to_string(),
            selected_input: input.selected_input,
            sources,
            delivery: input.delivery,
            context: context_metadata,
            prompt_bytes: encoded_len(&prompt, QUEUE_ADMISSION_BYTES as usize)?,
            file_reference_ordinals,
        };
        let header = encode(&frame, QUEUE_MANIFEST_BYTES).map_err(|error| match error {
            QueueFrozenSendError::OutgoingTooLarge => QueueFrozenSendError::MetadataTooLarge,
            other => other,
        })?;
        let size = frame
            .context
            .iter()
            .try_fold(
                HEADER_BYTES + header.len() + frame.prompt_bytes,
                |size, item| size.checked_add(item.bytes),
            )
            .ok_or(QueueFrozenSendError::OutgoingTooLarge)?;
        if size > QUEUE_ADMISSION_BYTES as usize {
            return Err(QueueFrozenSendError::OutgoingTooLarge);
        }
        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&header);
        for body in &bodies {
            write_queue_request_json(&mut bytes, body)
                .map_err(|_| QueueFrozenSendError::InvalidInput)?;
        }
        write_queue_request_json(&mut bytes, &prompt)
            .map_err(|_| QueueFrozenSendError::InvalidInput)?;
        let (context_ranges, prompt_range) =
            ranges(&frame, HEADER_BYTES + header.len(), bytes.len())?;
        validate_bodies(&bytes, &frame, &context_ranges, &prompt_range)?;
        Ok(Self {
            client: client.clone(),
            scope,
            metadata: frame,
            digest: hash(&bytes),
            bytes,
            context_ranges,
            prompt_range,
        })
    }

    /// Restore only after the caller reads and hashes the original spool chunks.
    /// The expected owner, prompt ID, configuration and whole-frame digest come
    /// from the retained operation, never from the currently selected composer.
    pub fn from_bytes(
        client: &ServerClient,
        expected: QueueFrozenSendOwner<'_>,
        bytes: Vec<u8>,
        digest: [u8; 32],
        hash: QueueSha256,
    ) -> Result<Self, QueueFrozenSendError> {
        if bytes.len() < HEADER_BYTES
            || bytes.len() > QUEUE_ADMISSION_BYTES as usize
            || &bytes[..8] != MAGIC
        {
            return Err(QueueFrozenSendError::InvalidInput);
        }
        if hash(&bytes) != digest {
            return Err(QueueFrozenSendError::DigestMismatch);
        }
        let header_len =
            u32::from_be_bytes(bytes[8..12].try_into().expect("fixed header slice")) as usize;
        if header_len > QUEUE_MANIFEST_BYTES {
            return Err(QueueFrozenSendError::MetadataTooLarge);
        }
        let header_end = HEADER_BYTES + header_len;
        let frame: FrameMetadata = serde_json::from_slice(
            bytes
                .get(HEADER_BYTES..header_end)
                .ok_or(QueueFrozenSendError::InvalidInput)?,
        )
        .map_err(|_| QueueFrozenSendError::InvalidInput)?;
        let scope = QueueScope::new(client, expected.session.clone(), expected.directory)
            .map_err(|_| QueueFrozenSendError::InvalidInput)?;
        if frame.endpoint != client.endpoint().as_str()
            || frame.session != expected.session.as_str()
            || frame.directory != expected.directory
            || frame.message_id != expected.message_id.as_str()
            || frame.sources.is_empty()
            || frame.sources.len() > 21
            || frame
                .sources
                .iter()
                .any(|source| !source.configuration().valid())
            || frame
                .sources
                .iter()
                .try_fold(0u64, |size, source| {
                    size.checked_add(source.structured_bytes())
                })
                .is_none_or(|size| size > QUEUE_PAYLOAD_BYTES as u64)
            || frame
                .sources
                .get(frame.selected_input)
                .is_none_or(|source| source.configuration().restore() != *expected.configuration)
        {
            return Err(QueueFrozenSendError::OwnerMismatch);
        }
        let (context_ranges, prompt_range) = ranges(&frame, header_end, bytes.len())?;
        validate_bodies(&bytes, &frame, &context_ranges, &prompt_range)?;
        Ok(Self {
            client: client.clone(),
            scope,
            metadata: frame,
            bytes,
            digest,
            context_ranges,
            prompt_range,
        })
    }

    /// Complete immutable frame for the caller's existing private spool worker.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Transfer the frame to its spool owner without cloning the large buffer.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
    /// Whole-frame SHA-256 calculated by the supplied destination implementation.
    pub fn sha256(&self) -> [u8; 32] {
        self.digest
    }
    /// Captured real session. No selection lookup occurs.
    pub fn session(&self) -> &SessionId {
        self.scope.session()
    }
    /// Exact captured directory spelling.
    pub fn directory(&self) -> &str {
        self.scope.directory()
    }
    /// Original retained identity, reconstructed without minting a new ID.
    pub fn message_id(&self) -> MessageId {
        MessageId::new(self.metadata.message_id.clone())
    }
    /// Original configuration of the explicitly selected source occurrence.
    pub fn configuration(&self) -> QueuedSendConfig {
        self.metadata.sources[self.metadata.selected_input]
            .configuration()
            .restore()
    }
    /// Source provenance retained through restart. This is not transfer proof.
    pub fn source_kinds(&self) -> impl Iterator<Item = QueueFrozenSourceKind> + '_ {
        self.metadata.sources.iter().map(SourceMetadata::kind)
    }
    /// Original delivery choice, including an omitted engine default.
    pub fn delivery(&self) -> Option<Delivery> {
        self.metadata.delivery
    }
    /// Exact frozen synthetic IDs in dispatch order.
    pub fn context_ids(&self) -> Vec<MessageId> {
        self.metadata
            .context
            .iter()
            .map(|item| MessageId::new(item.id.clone()))
            .collect()
    }
    /// Exact model-facing JSON, useful for immutable outgoing-size checks.
    pub fn prompt_body(&self) -> &[u8] {
        &self.bytes[self.prompt_range.clone()]
    }
    /// Exact synthetic request JSON at this zero-based dispatch position.
    pub fn context_body(&self, index: usize) -> Option<&[u8]> {
        self.context_ranges
            .get(index)
            .map(|range| &self.bytes[range.clone()])
    }

    /// One explicit attempt. `admitted` must be an observed leading context run.
    /// Earlier selections and admissions survive later failures. Each request
    /// copies only its body into reqwest; the retained frame stays immutable.
    pub async fn dispatch(&self, admitted: &[MessageId]) -> Result<PromptReceipt, SendError> {
        self.dispatch_step(QueueFrozenDispatchStep::Model, admitted)
            .await?;
        if self.configuration().agent.is_some() {
            self.dispatch_step(QueueFrozenDispatchStep::Agent, admitted)
                .await?;
        }
        let contexts = self.context_ids();
        for index in admitted.len()..contexts.len() {
            self.dispatch_step(QueueFrozenDispatchStep::Context(index), &contexts[..index])
                .await?;
        }
        self.dispatch_step(QueueFrozenDispatchStep::Prompt, &contexts)
            .await?
            .ok_or_else(|| self.failure(SendStage::Validation, !contexts.is_empty(), malformed()))
    }

    /// Make exactly one explicit request with the captured client and body.
    /// `admitted` is the caller's observed or receipted leading context run.
    /// Context and prompt steps reject gaps before any request. Selection steps
    /// return `None`; admissions return their validated receipt. This method
    /// neither observes nor retries, and a failed request leaves the frame intact.
    pub async fn dispatch_step(
        &self,
        step: QueueFrozenDispatchStep,
        admitted: &[MessageId],
    ) -> Result<Option<PromptReceipt>, SendError> {
        if admitted.len() > self.metadata.context.len()
            || admitted
                .iter()
                .zip(&self.metadata.context)
                .any(|(id, item)| id.as_str() != item.id)
            || matches!(step, QueueFrozenDispatchStep::Context(index) if index != admitted.len() || index >= self.metadata.context.len())
            || (step == QueueFrozenDispatchStep::Prompt
                && admitted.len() != self.metadata.context.len())
            || (step == QueueFrozenDispatchStep::Agent && self.configuration().agent.is_none())
        {
            return Err(self.failure(SendStage::Validation, !admitted.is_empty(), malformed()));
        }
        let partial = !admitted.is_empty();
        match step {
            QueueFrozenDispatchStep::Model => {
                let config = self.configuration();
                let model = ModelChoice {
                    provider_id: config.provider_id,
                    model_id: config.model_id,
                    variant: config.variant,
                };
                checked(
                    self.client.model_request(self.session(), &model),
                    StatusCode::NO_CONTENT,
                )
                .await
                .map_err(|error| self.failure(SendStage::Model, partial, error))?;
                Ok(None)
            }
            QueueFrozenDispatchStep::Agent => {
                let config = self.configuration();
                let agent = config
                    .agent
                    .expect("agent step validated before requesting");
                checked(
                    self.client.agent_request(self.session(), &agent),
                    StatusCode::NO_CONTENT,
                )
                .await
                .map_err(|error| self.failure(SendStage::Agent, partial, error))?;
                Ok(None)
            }
            QueueFrozenDispatchStep::Context(index) => {
                let item = &self.metadata.context[index];
                self.admit(
                    "synthetic",
                    &item.id,
                    self.context_body(index).expect("validated context range"),
                )
                .await
                .map(Some)
                .map_err(|error| self.failure(SendStage::Context { index }, partial, error))
            }
            QueueFrozenDispatchStep::Prompt => self
                .admit("prompt", &self.metadata.message_id, self.prompt_body())
                .await
                .map(Some)
                .map_err(|error| self.failure(SendStage::Prompt, partial, error)),
        }
    }

    fn failure(&self, stage: SendStage, partial: bool, source: TransportError) -> SendError {
        let admitting = matches!(stage, SendStage::Context { .. } | SendStage::Prompt);
        let definite = matches!(
            source,
            TransportError::AuthenticationRequired
                | TransportError::Http {
                    status: 400 | 401 | 403 | 404
                }
        );
        let admission = if stage == SendStage::Validation {
            PromptAdmission::NotAttempted
        } else if admitting && !definite {
            PromptAdmission::Unknown
        } else if partial {
            PromptAdmission::Partial
        } else if stage == SendStage::Prompt {
            PromptAdmission::Rejected
        } else {
            PromptAdmission::NotAttempted
        };
        SendError {
            session_id: self.session().clone(),
            message_id: self.message_id(),
            stage,
            admission,
            source,
        }
    }

    fn request(&self, operation: &str, body: &[u8]) -> RequestBuilder {
        self.client
            .mutation_request(self.session(), operation)
            .header("content-type", "application/json")
            .body(body.to_vec())
    }

    async fn admit(
        &self,
        operation: &str,
        id: &str,
        body: &[u8],
    ) -> Result<PromptReceipt, TransportError> {
        #[derive(Deserialize)]
        struct Reply {
            data: InboxItem,
        }
        let response = checked(self.request(operation, body), StatusCode::OK).await?;
        let reply: Reply = bounded_json(response, QUEUE_ADMISSION_BYTES as usize).await?;
        receipt(
            &reply.data,
            self.session(),
            id,
            if operation == "prompt" {
                "user"
            } else {
                "synthetic"
            },
        )
    }

    /// Read-only reconciliation. Absence never authorizes replay. A gap followed
    /// by an observed synthetic returns `OutOfOrder` and must remain blocked.
    pub async fn observe(&self) -> Result<SendObservation, TransportError> {
        #[derive(Deserialize)]
        struct Reply {
            data: Vec<InboxItem>,
        }
        let reply: Reply = bounded_json(
            checked(self.client.inbox_request(self.session()), StatusCode::OK).await?,
            QUEUE_ADMISSION_BYTES as usize,
        )
        .await?;
        let mut ids = HashSet::new();
        if reply.data.iter().any(|item| {
            !message_id(&item.id)
                || item.session_id != self.session().as_str()
                || !matches!(
                    item.kind.as_str(),
                    "user" | "synthetic" | "compaction" | "move"
                )
                || !ids.insert(&item.id)
        }) {
            return Err(malformed());
        }
        match self
            .observe_item(&reply.data, &self.metadata.message_id, "user")
            .await?
        {
            PromptObservation::Pending(receipt) => return Ok(SendObservation::Pending(receipt)),
            PromptObservation::Delivered(message) => {
                return Ok(SendObservation::Delivered(message));
            }
            PromptObservation::NotObserved => {}
        }
        let mut admitted = Vec::new();
        let mut missing = false;
        for item in &self.metadata.context {
            let observed = !matches!(
                self.observe_item(&reply.data, &item.id, "synthetic")
                    .await?,
                PromptObservation::NotObserved
            );
            if observed && missing {
                return Ok(SendObservation::OutOfOrder);
            }
            if observed {
                admitted.push(MessageId::new(item.id.clone()));
            } else {
                missing = true;
            }
        }
        Ok(SendObservation::Missing { admitted })
    }

    async fn observe_item(
        &self,
        inbox: &[InboxItem],
        id: &str,
        kind: &str,
    ) -> Result<PromptObservation, TransportError> {
        if let Some(item) = inbox.iter().find(|item| item.id == id) {
            return receipt(item, self.session(), id, kind).map(PromptObservation::Pending);
        }
        match self.history(id).await {
            Ok(message)
                if message.info.id.as_str() == id
                    && message.info.role
                        == if kind == "user" {
                            Role::User
                        } else {
                            Role::Synthetic
                        } =>
            {
                Ok(PromptObservation::Delivered(Box::new(message)))
            }
            Ok(_) => Err(malformed()),
            Err(TransportError::Http { status: 404 }) => Ok(PromptObservation::NotObserved),
            Err(TransportError::Malformed(_)) => Err(malformed()),
            Err(error) => Err(error),
        }
    }

    async fn history(
        &self,
        id: &str,
    ) -> Result<openchamber_native_core::transcript::Message, TransportError> {
        // The ordinary whole-message helper has no response-byte bound. Keep its
        // wire/projection semantics while bounding this active recovery operation.
        let url = self
            .client
            .session_message_url(self.session(), &MessageId::new(id.to_owned()));
        let response = checked(self.client.authorized(Method::GET, url), StatusCode::OK).await?;
        let single: crate::wire::Single<crate::wire::MessageEnvelope> =
            bounded_json(response, QUEUE_ADMISSION_BYTES as usize).await?;
        if single.data.id != id
            || (single.data.kind == "user"
                && !single.data.body.get("text").is_some_and(Value::is_string))
        {
            return Err(malformed());
        }
        crate::projection::message(single.data)
            .map_err(|_| malformed())?
            .ok_or_else(malformed)
    }
}

/// Expected retained operation bindings for reconstruction. Original payload
/// spool references remain with the caller, including display/rollback content.
pub struct QueueFrozenSendOwner<'a> {
    pub session: &'a SessionId,
    pub directory: &'a str,
    pub message_id: &'a MessageId,
    pub configuration: &'a QueuedSendConfig,
}

impl fmt::Debug for QueueFrozenSendOwner<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueFrozenSendOwner")
            .field("session", self.session)
            .field("message_id", self.message_id)
            .finish_non_exhaustive()
    }
}

fn binding<'a>(
    files: &'a [QueuePreparedFile],
    source: &PreparedFileSource,
) -> Result<(usize, &'a QueuePreparedFile), QueueFrozenSendError> {
    let mut matches = files
        .iter()
        .enumerate()
        .filter(|(_, file)| &file.source == source);
    let binding = matches.next().ok_or(QueueFrozenSendError::FileMismatch)?;
    if matches.next().is_some() {
        return Err(QueueFrozenSendError::FileMismatch);
    }
    Ok(binding)
}

fn local_structured_bytes(item: &QueueLocalItem) -> Result<u64, QueueFrozenSendError> {
    fn json<T: Serialize + ?Sized>(value: &T) -> Result<u64, QueueFrozenSendError> {
        compact_queue_json_len(value)
            .map(|size| size as u64)
            .map_err(|_| QueueFrozenSendError::StructuredTooLarge)
    }
    let parts = &item.prompt_parts;
    let mut size = item.text.len() as u64 + json(&parts.agents)? + json(&parts.skills)?;
    if !parts.file_references.is_empty() {
        size += json(&parts.file_references)?;
    }
    if let Some(metadata) = &parts.metadata {
        size += json(metadata)?;
    }
    for context in &parts.context {
        size +=
            context.text.len() as u64 + context.description.as_deref().map_or(0, str::len) as u64;
        if let Some(metadata) = &context.metadata {
            size += json(metadata)?;
        }
    }
    for file in &item.files {
        size += file.name.len() as u64 + file.description.as_deref().map_or(0, str::len) as u64;
        if let Some(mention) = &file.mention {
            size += json(mention)?;
        }
    }
    if size > QUEUE_PAYLOAD_BYTES as u64 {
        return Err(QueueFrozenSendError::StructuredTooLarge);
    }
    Ok(size)
}

fn ranges(
    frame: &FrameMetadata,
    mut offset: usize,
    length: usize,
) -> Result<(Vec<Range<usize>>, Range<usize>), QueueFrozenSendError> {
    if !message_id(&frame.message_id) || frame.context.len() > QUEUE_MANIFEST_ENTRIES {
        return Err(QueueFrozenSendError::InvalidInput);
    }
    let mut ids = HashSet::from([frame.message_id.as_str()]);
    let mut ranges = Vec::with_capacity(frame.context.len());
    for item in &frame.context {
        if !message_id(&item.id) || !ids.insert(&item.id) || item.bytes == 0 {
            return Err(QueueFrozenSendError::InvalidInput);
        }
        let end = offset
            .checked_add(item.bytes)
            .filter(|end| *end <= length)
            .ok_or(QueueFrozenSendError::InvalidInput)?;
        ranges.push(offset..end);
        offset = end;
    }
    let end = offset
        .checked_add(frame.prompt_bytes)
        .filter(|end| *end == length)
        .ok_or(QueueFrozenSendError::InvalidInput)?;
    if frame.prompt_bytes == 0 {
        return Err(QueueFrozenSendError::InvalidInput);
    }
    Ok((ranges, offset..end))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckMention {
    start: u64,
    end: u64,
    #[serde(rename = "text")]
    _text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckFile {
    uri: String,
    name: String,
    #[serde(rename = "description")]
    _description: Option<String>,
    mention: Option<CheckMention>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckAgent {
    name: String,
    mention: Option<CheckMention>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckSkill {
    id: String,
    mention: Option<CheckMention>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckPrompt {
    id: String,
    text: String,
    #[serde(default)]
    files: Vec<CheckFile>,
    #[serde(default)]
    agents: Vec<CheckAgent>,
    #[serde(default)]
    skills: Vec<CheckSkill>,
    metadata: Option<Map<String, Value>>,
    delivery: Option<Delivery>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckContext {
    id: String,
    text: String,
    #[serde(rename = "description")]
    _description: Option<String>,
    metadata: Option<Map<String, Value>>,
    delivery: Option<Delivery>,
    resume: bool,
}

fn validate_bodies(
    bytes: &[u8],
    frame: &FrameMetadata,
    context: &[Range<usize>],
    prompt: &Range<usize>,
) -> Result<(), QueueFrozenSendError> {
    // Parse one body at a time. No decoded prompt/context tree is retained.
    for (range, item) in context.iter().zip(&frame.context) {
        let body: CheckContext = serde_json::from_slice(&bytes[range.clone()])
            .map_err(|_| QueueFrozenSendError::InvalidInput)?;
        if body.id != item.id
            || body.delivery != frame.delivery
            || body.resume
            || js_trim(&body.text).is_empty()
        {
            return Err(QueueFrozenSendError::InvalidInput);
        }
        validate_metadata(body.metadata.as_ref())?;
    }
    let body: CheckPrompt = serde_json::from_slice(&bytes[prompt.clone()])
        .map_err(|_| QueueFrozenSendError::InvalidInput)?;
    if body.id != frame.message_id
        || body.delivery != frame.delivery
        || (js_trim(&body.text).is_empty() && body.files.is_empty() && context.is_empty())
        || body.files.len() + body.agents.len() + body.skills.len() + context.len()
            > QUEUE_MANIFEST_ENTRIES
    {
        return Err(QueueFrozenSendError::InvalidInput);
    }
    validate_metadata(body.metadata.as_ref())?;
    let check_mention = |value: &Option<CheckMention>| -> Result<(), QueueFrozenSendError> {
        if let Some(value) = value
            && (value.start > value.end || value.end > MAX_SAFE_INTEGER)
        {
            return Err(QueueFrozenSendError::InvalidInput);
        }
        Ok(())
    };
    if frame.file_reference_ordinals.len() > body.files.len()
        || frame
            .file_reference_ordinals
            .iter()
            .any(|ordinal| *ordinal >= body.files.len())
        || frame
            .file_reference_ordinals
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
    {
        return Err(QueueFrozenSendError::FileMismatch);
    }
    for (ordinal, file) in body.files.iter().enumerate() {
        let reference = frame
            .file_reference_ordinals
            .binary_search(&ordinal)
            .is_ok();
        if file.name.is_empty()
            || (reference
                && (!queue_file_uri(&file.uri)
                    || file.name.len() > 4096
                    || file.name.chars().any(char::is_control)))
            || (!reference && !prepared_uri(&file.uri))
        {
            return Err(QueueFrozenSendError::FileMismatch);
        }
        check_mention(&file.mention)?;
    }
    for agent in &body.agents {
        if !identifier(&agent.name) {
            return Err(QueueFrozenSendError::InvalidInput);
        }
        check_mention(&agent.mention)?;
    }
    for skill in &body.skills {
        if !identifier(&skill.id) {
            return Err(QueueFrozenSendError::InvalidInput);
        }
        check_mention(&skill.mention)?;
    }
    Ok(())
}

fn validate_metadata(value: Option<&Map<String, Value>>) -> Result<(), QueueFrozenSendError> {
    fn valid(value: &Value, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match value {
            Value::Array(values) => values.iter().all(|value| valid(value, depth + 1)),
            Value::Object(values) => values.values().all(|value| valid(value, depth + 1)),
            Value::Number(number) => number.as_f64().is_some_and(f64::is_finite),
            _ => true,
        }
    }
    // The retained frame's actual bytes and digest are already bounded. Do not
    // re-encode old metadata with today's number notation to decide its size.
    // New captures measure their exact emitted bodies before frame allocation.
    match value {
        None => Ok(()),
        Some(value) if value.values().all(|value| valid(value, 1)) => Ok(()),
        Some(_) => Err(QueueFrozenSendError::InvalidInput),
    }
}

fn json_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| json_equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| json_equal(a, b)))
        }
        _ => left == right,
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.encode_utf16().count() <= 4096
        && !value.chars().any(char::is_control)
}

fn message_id(value: &str) -> bool {
    value.starts_with("msg_")
        && (5..=124).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn prepared_uri(value: &str) -> bool {
    if value.len() > 128 * 1024
        || !value.starts_with("file:///")
        || value.chars().any(char::is_control)
    {
        return false;
    }
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "file"
            && url.host_str().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() != "/"
    })
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|ch: char| ch == '\u{feff}' || ch.is_whitespace() && ch != '\u{85}')
}

struct Counter {
    bytes: usize,
    limit: usize,
}
impl io::Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes {
            return Err(io::Error::other("encoded queue send limit"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encoded_len(value: &impl Serialize, limit: usize) -> Result<usize, QueueFrozenSendError> {
    let mut count = Counter { bytes: 0, limit };
    write_queue_request_json(&mut count, value)
        .map_err(|_| QueueFrozenSendError::OutgoingTooLarge)?;
    Ok(count.bytes)
}

fn encode(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, QueueFrozenSendError> {
    let mut bytes = Vec::with_capacity(encoded_len(value, limit)?);
    write_queue_request_json(&mut bytes, value).map_err(|_| QueueFrozenSendError::InvalidInput)?;
    Ok(bytes)
}

#[derive(Deserialize)]
struct InboxItem {
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(rename = "type")]
    kind: String,
    time: Option<Created>,
    delivery: Option<Delivery>,
}
#[derive(Deserialize)]
struct Created {
    created: u64,
}

fn receipt(
    item: &InboxItem,
    session: &SessionId,
    id: &str,
    kind: &str,
) -> Result<PromptReceipt, TransportError> {
    if item.id != id || item.session_id != session.as_str() || item.kind != kind {
        return Err(malformed());
    }
    Ok(PromptReceipt {
        session_id: session.clone(),
        message_id: MessageId::new(id.to_owned()),
        created: item.time.as_ref().ok_or_else(malformed)?.created,
        delivery: item.delivery.ok_or_else(malformed)?,
    })
}

fn malformed() -> TransportError {
    TransportError::Malformed("invalid frozen queue send response".to_owned())
}

async fn checked(
    request: RequestBuilder,
    expected: StatusCode,
) -> Result<Response, TransportError> {
    let response = request.send().await.map_err(TransportError::from_request)?;
    let status = response.status();
    if status.is_redirection() {
        return Err(TransportError::Redirected);
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(TransportError::AuthenticationRequired);
    }
    if status != expected {
        return Err(TransportError::Http {
            status: status.as_u16(),
        });
    }
    Ok(response)
}

async fn bounded_json<T: serde::de::DeserializeOwned>(
    mut response: Response,
    limit: usize,
) -> Result<T, TransportError> {
    if !response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
        || response
            .content_length()
            .is_some_and(|size| size > limit as u64)
    {
        return Err(malformed());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(TransportError::from_request)?
    {
        if chunk.len() > limit - bytes.len() {
            return Err(malformed());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| malformed())
}

#[cfg(test)]
#[path = "queue_frozen_send_tests.rs"]
mod tests;
