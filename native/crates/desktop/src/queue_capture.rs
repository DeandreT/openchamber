//! Immutable local queue captures. Server upload identities never name local files.
//!
//! A capture is published only after its exact held chunks have durable receipts
//! and the worker has verified the complete byte stream. Expanded native sources
//! exist only before that publication or during an explicit materialization.

use std::collections::HashSet;
use std::fmt;

use openchamber_native_core::draft::{Draft, DraftKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::attachment_spool::{BlobDescriptor, BlobId};
use crate::draft_storage::StoredDraft;
#[cfg(test)]
use crate::mutation_journal::Owner;
use crate::mutation_journal::{JournalError, MAX_ENTRIES, identifier};

pub(crate) const QUEUE_CHUNK_BYTES: usize = 20 * 1024 * 1024;
pub(crate) const QUEUE_CAPTURE_BYTES: u64 = 50 * 1024 * 1024;
pub(crate) const QUEUE_BODY_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const QUEUE_SEND_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct QueueOperationId(String);

impl QueueOperationId {
    pub(crate) fn new(value: &str) -> Result<Self, JournalError> {
        if !(8..=128).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return Err(JournalError::Invalid("invalid queue operation identity"));
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for QueueOperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QueueOperationId([redacted])")
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct QueueObjectId(String);

impl QueueObjectId {
    pub(crate) fn new(value: &str) -> Result<Self, JournalError> {
        identifier(value)?;
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for QueueObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QueueObjectId([redacted])")
    }
}

/// Exact UTF-8 JSON, including omission choices and original number spelling.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct QueueJson(String);

impl QueueJson {
    pub(crate) fn new(bytes: Vec<u8>) -> Result<Self, JournalError> {
        let json = Self(
            String::from_utf8(bytes)
                .map_err(|_| JournalError::Invalid("queue body is not UTF-8"))?,
        );
        json.validate()?;
        Ok(json)
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub(crate) fn validate(&self) -> Result<(), JournalError> {
        if self.0.is_empty() || self.0.len() > QUEUE_BODY_BYTES {
            return Err(JournalError::Invalid("queue body size limit exceeded"));
        }
        serde_json::from_str::<serde::de::IgnoredAny>(&self.0)
            .map_err(|_| JournalError::Invalid("malformed retained queue JSON"))?;
        Ok(())
    }

    pub(crate) fn value(&self) -> Result<serde_json::Value, JournalError> {
        self.validate()?;
        serde_json::from_str(&self.0)
            .map_err(|_| JournalError::Invalid("malformed retained queue JSON"))
    }
}

impl fmt::Debug for QueueJson {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueueJson")
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// Queue chunks have their own catalog and never become composer attachments.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueBlob {
    id: String,
    byte_len: u64,
    sha256: [u8; 32],
}

impl QueueBlob {
    pub(crate) fn from_descriptor(descriptor: BlobDescriptor) -> Self {
        Self {
            id: descriptor.id().as_str().to_owned(),
            byte_len: descriptor.byte_len(),
            sha256: *descriptor.sha256(),
        }
    }

    pub(crate) fn descriptor(&self) -> Result<BlobDescriptor, JournalError> {
        if self.byte_len == 0 || self.byte_len > QUEUE_CHUNK_BYTES as u64 {
            return Err(JournalError::Invalid("invalid queue chunk length"));
        }
        Ok(BlobDescriptor::new(
            BlobId::parse(&self.id)
                .map_err(|_| JournalError::Invalid("invalid local queue blob identity"))?,
            self.byte_len,
            self.sha256,
        ))
    }
}

impl fmt::Debug for QueueBlob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueueBlob")
            .field("byte_len", &self.byte_len)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueueObjectKind {
    Payload,
    NativeCompanion,
    FrozenSend,
}

impl QueueObjectKind {
    pub(crate) fn byte_limit(self) -> u64 {
        match self {
            Self::Payload | Self::NativeCompanion => QUEUE_CAPTURE_BYTES,
            Self::FrozenSend => QUEUE_SEND_BYTES,
        }
    }
}

/// Array position is the ordinal. Local random labels are unrelated to server IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueSpoolRef {
    total_bytes: u64,
    sha256: [u8; 32],
    chunks: Vec<QueueBlob>,
}

impl QueueSpoolRef {
    pub(crate) fn new(
        total_bytes: u64,
        sha256: [u8; 32],
        chunks: Vec<BlobDescriptor>,
        kind: QueueObjectKind,
    ) -> Result<Self, JournalError> {
        let reference = Self {
            total_bytes,
            sha256,
            chunks: chunks.into_iter().map(QueueBlob::from_descriptor).collect(),
        };
        reference.validate(kind)?;
        Ok(reference)
    }

    pub(crate) fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
    pub(crate) fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
    pub(crate) fn chunks(&self) -> &[QueueBlob] {
        &self.chunks
    }

    pub(crate) fn validate(&self, kind: QueueObjectKind) -> Result<(), JournalError> {
        if self.total_bytes == 0
            || self.total_bytes > kind.byte_limit()
            || self.chunks.is_empty()
            || self.chunks.len() > MAX_ENTRIES
        {
            return Err(JournalError::Invalid(
                "queue capture size or chunk count limit exceeded",
            ));
        }
        let mut ids = HashSet::new();
        let mut total = 0u64;
        for chunk in &self.chunks {
            let descriptor = chunk.descriptor()?;
            if !ids.insert(*descriptor.id()) {
                return Err(JournalError::Invalid("duplicate queue chunk identity"));
            }
            total = total
                .checked_add(descriptor.byte_len())
                .ok_or(JournalError::Invalid("queue capture length overflow"))?;
        }
        if total != self.total_bytes {
            return Err(JournalError::Invalid(
                "queue chunk lengths differ from capture",
            ));
        }
        Ok(())
    }
}

/// Complete native source, including source-document IDs, before spool publication.
/// Its serializer is the existing lossless native draft representation.
#[derive(Clone)]
pub(crate) struct QueueNativeSource {
    pub(crate) owner: DraftKey,
    pub(crate) draft: Draft,
}

impl Serialize for QueueNativeSource {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        StoredDraft::capture_with(&self.owner, &self.draft, &[], |text| text).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for QueueNativeSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (owner, draft) = StoredDraft::deserialize(deserializer)?
            .into_draft()
            .map_err(serde::de::Error::custom)?;
        Ok(Self { owner, draft })
    }
}

impl fmt::Debug for QueueNativeSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QueueNativeSource([redacted])")
    }
}

/// Active bytes only. Journal records contain the reference, never this buffer.
pub(crate) struct QueueCaptureBytes {
    bytes: Vec<u8>,
    reference: QueueSpoolRef,
}

impl QueueCaptureBytes {
    pub(crate) fn new(
        kind: QueueObjectKind,
        bytes: Vec<u8>,
        ids: &[BlobId],
    ) -> Result<Self, JournalError> {
        if bytes.is_empty()
            || bytes.len() as u64 > kind.byte_limit()
            || ids.len() != bytes.len().div_ceil(QUEUE_CHUNK_BYTES)
        {
            return Err(JournalError::Invalid(
                "invalid queue capture bytes or labels",
            ));
        }
        let descriptors = bytes
            .chunks(QUEUE_CHUNK_BYTES)
            .zip(ids)
            .map(|(chunk, id)| {
                BlobDescriptor::new(*id, chunk.len() as u64, Sha256::digest(chunk).into())
            })
            .collect();
        let reference = QueueSpoolRef::new(
            bytes.len() as u64,
            Sha256::digest(&bytes).into(),
            descriptors,
            kind,
        )?;
        Ok(Self { bytes, reference })
    }

    #[cfg(test)]
    pub(crate) fn native_source(
        source: &QueueNativeSource,
        ids: &[BlobId],
    ) -> Result<Self, JournalError> {
        crate::draft_storage::validate(&mut 0, &source.draft)?;
        Owner::capture(&source.owner).into_key()?;
        let bytes = serde_json::to_vec(source)
            .map_err(|_| JournalError::Invalid("native queue companion cannot be encoded"))?;
        Self::new(QueueObjectKind::NativeCompanion, bytes, ids)
    }

    pub(crate) fn reference(&self) -> &QueueSpoolRef {
        &self.reference
    }
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn chunk(&self, ordinal: usize) -> Option<&[u8]> {
        self.bytes.chunks(QUEUE_CHUNK_BYTES).nth(ordinal)
    }
}

impl fmt::Debug for QueueCaptureBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueueCaptureBytes")
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueChunkHold {
    pub(crate) operation: QueueOperationId,
    pub(crate) object: QueueObjectId,
    pub(crate) generation: u64,
    pub(crate) ordinal: usize,
    pub(crate) chunk: QueueBlob,
}

/// Exact verification request. This metadata alone proves no file was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueueVerification {
    pub(crate) operation: QueueOperationId,
    pub(crate) object: QueueObjectId,
    pub(crate) generation: u64,
    pub(crate) reference: QueueSpoolRef,
}

pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
