//! Shared prepared composer inputs for enqueue and prefix operations.
//!
//! The version-1 binary envelope contains an 8-byte magic, a big-endian u32
//! metadata length, strict JSON metadata, then the original local capture bytes.
//! Attachment bytes remain in their existing spool. This envelope grants no
//! filesystem access, queue index proof, transfer acknowledgement or dispatch.

use std::collections::{HashMap, HashSet};
use std::fmt;

use gpui_kit::{AsyncWindowContext, WeakEntity};
use openchamber_native_transport::attachment::MAX_PREPARED_ATTACHMENT_BYTES;
use openchamber_native_transport::queue_manifest::QueueLocalCapture;
use serde::{Deserialize, Serialize};

use crate::attachment_spool::{BlobDescriptor, BlobId};
use crate::mutation_journal::{JournalError, MAX_ENTRIES};
use crate::queue_capture::{
    QUEUE_BODY_BYTES, QUEUE_CAPTURE_BYTES, QueueObjectId, QueueObjectKind, QueueOperationId, sha256,
};
use crate::queue_payload_io::{self, QueueCaptureFailure};
use crate::queue_storage::{
    QueueDerivedFileState, QueueNativeFileDescriptor, QueueObject, QueueOperation, QueueStorage,
};
use crate::workspace::Workspace;

const MAGIC: &[u8; 8] = b"OCCPIN01";
const PREFIX_BYTES: usize = 12;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputMetadata {
    version: u32,
    generation: u64,
    revision: u64,
    source_sha256: [u8; 32],
    files: Vec<QueueNativeFileDescriptor>,
    capture_len: u64,
    capture_sha256: [u8; 32],
}

/// An already projected capture and its occurrence-ordered local byte bindings.
/// Callers retain preparation and catalog ownership separately.
pub(crate) struct PreparedComposerInput {
    pub(crate) capture: QueueLocalCapture,
    pub(crate) files: Vec<BlobDescriptor>,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) source_sha256: [u8; 32],
}

impl fmt::Debug for PreparedComposerInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreparedComposerInput([redacted])")
    }
}

impl PreparedComposerInput {
    pub(crate) fn new(
        capture: QueueLocalCapture,
        files: Vec<BlobDescriptor>,
        generation: u64,
        revision: u64,
        source_sha256: [u8; 32],
    ) -> Result<Self, QueueCaptureFailure> {
        let input = Self {
            capture,
            files,
            generation,
            revision,
            source_sha256,
        };
        input.checked_metadata()?;
        Ok(input)
    }

    /// Serializes metadata only. The capture's exact original JSON follows it
    /// unchanged, including omission choices and numeric spelling.
    pub(crate) fn encode(&self) -> Result<Vec<u8>, QueueCaptureFailure> {
        let metadata = self.checked_metadata()?;
        let length = envelope_len(metadata.len(), self.capture.bytes().len())?;
        let metadata_len =
            u32::try_from(metadata.len()).map_err(|_| QueueCaptureFailure::Invalid)?;
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&metadata_len.to_be_bytes());
        bytes.extend_from_slice(&metadata);
        bytes.extend_from_slice(self.capture.bytes());
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8], directory: &str) -> Result<Self, QueueCaptureFailure> {
        if bytes.len() < PREFIX_BYTES
            || bytes.len() as u64 > QUEUE_CAPTURE_BYTES
            || &bytes[..8] != MAGIC
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        let metadata_len = u32::from_be_bytes(
            bytes[8..PREFIX_BYTES]
                .try_into()
                .map_err(|_| QueueCaptureFailure::Invalid)?,
        ) as usize;
        if metadata_len == 0 || metadata_len > QUEUE_BODY_BYTES {
            return Err(QueueCaptureFailure::Invalid);
        }
        let start = PREFIX_BYTES
            .checked_add(metadata_len)
            .ok_or(QueueCaptureFailure::Invalid)?;
        let metadata: InputMetadata = serde_json::from_slice(
            bytes
                .get(PREFIX_BYTES..start)
                .ok_or(QueueCaptureFailure::Invalid)?,
        )
        .map_err(|_| QueueCaptureFailure::Invalid)?;
        let capture_len =
            usize::try_from(metadata.capture_len).map_err(|_| QueueCaptureFailure::Invalid)?;
        if metadata.version != 1
            || metadata.files.len() > MAX_ENTRIES
            || envelope_len(metadata_len, capture_len)? != bytes.len()
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        let files = metadata
            .files
            .iter()
            .map(|file| {
                let id = BlobId::parse(&file.id).map_err(|_| QueueCaptureFailure::Invalid)?;
                Ok(BlobDescriptor::new(id, file.byte_len, file.sha256))
            })
            .collect::<Result<Vec<_>, QueueCaptureFailure>>()?;
        let capture = QueueLocalCapture::from_bytes(
            bytes
                .get(start..)
                .ok_or(QueueCaptureFailure::Invalid)?
                .to_vec(),
            directory,
            sha256,
        )
        .map_err(|_| QueueCaptureFailure::Invalid)?;
        if capture.sha256() != metadata.capture_sha256 {
            return Err(QueueCaptureFailure::Invalid);
        }
        Self::new(
            capture,
            files,
            metadata.generation,
            metadata.revision,
            metadata.source_sha256,
        )
    }

    /// Checks byte roots and context order through the common queue ledger.
    /// Display anchors are already global and remain untouched here.
    pub(crate) fn validate(
        &self,
        queue: &QueueStorage,
        id: &QueueOperationId,
    ) -> Result<(), QueueCaptureFailure> {
        let operation = queue.operation(id).ok_or(QueueCaptureFailure::Invalid)?;
        self.validate_operation(
            operation,
            &queue.composer_context_ids(id)?,
            &queue.composer_file_descriptors(id)?,
        )
    }

    fn validate_operation(
        &self,
        operation: &QueueOperation,
        contexts: &[String],
        roots: &[QueueNativeFileDescriptor],
    ) -> Result<(), QueueCaptureFailure> {
        self.checked_metadata()?;
        let preparation = operation
            .composer_preparation()
            .ok_or(QueueCaptureFailure::Invalid)?;
        let owner = operation.owner().draft_key()?;
        if !operation.has_compound_source()
            || self.capture.directory() != operation.owner().directory()
            || preparation.owner()? != owner
            || operation.source_sha256() != Some(&self.source_sha256)
            || preparation.source_sha256 != self.source_sha256
            || preparation.generation != self.generation
            || preparation.revision != self.revision
            || self.capture.item().send_config != operation.configuration().restore()
            || self
                .capture
                .context_ids()
                .iter()
                .map(|id| id.as_str())
                .ne(contexts.iter().map(String::as_str))
            || preparation
                .files
                .iter()
                .any(|file| file.state != QueueDerivedFileState::Cataloged)
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        let mut canonical = HashMap::new();
        for root in roots {
            let id = BlobId::parse(&root.id).map_err(|_| QueueCaptureFailure::Invalid)?;
            let descriptor = BlobDescriptor::new(id, root.byte_len, root.sha256);
            if root.byte_len > MAX_PREPARED_ATTACHMENT_BYTES as u64
                || canonical
                    .insert(id, descriptor)
                    .is_some_and(|old| old != descriptor)
            {
                return Err(QueueCaptureFailure::Invalid);
            }
        }
        let derived: HashMap<_, _> = preparation
            .files
            .iter()
            .map(|file| (file.local_attachment_id.as_str(), file))
            .collect();
        if derived.len() != preparation.files.len() {
            return Err(QueueCaptureFailure::Invalid);
        }
        let mut seen = HashSet::new();
        for (descriptor, file) in self.files.iter().zip(&self.capture.item().files) {
            if canonical.get(descriptor.id()) != Some(descriptor) {
                return Err(QueueCaptureFailure::Invalid);
            }
            if let Some(planned) = derived.get(descriptor.id().as_str()) {
                if planned.descriptor()? != *descriptor
                    || planned.name != file.name
                    || planned.mime_type != file.mime_type
                {
                    return Err(QueueCaptureFailure::Invalid);
                }
                seen.insert(descriptor.id().as_str());
            }
        }
        if derived.keys().any(|id| !seen.contains(id)) {
            return Err(QueueCaptureFailure::Invalid);
        }
        Ok(())
    }

    fn checked_metadata(&self) -> Result<Vec<u8>, QueueCaptureFailure> {
        if self.generation == 0
            || self.generation == u64::MAX
            || self.revision == 0
            || self.revision == u64::MAX
            || self.files.len() > MAX_ENTRIES
            || self.files.len() != self.capture.item().files.len()
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        let mut descriptors = HashMap::new();
        for (descriptor, file) in self.files.iter().zip(&self.capture.item().files) {
            if descriptor.byte_len() > MAX_PREPARED_ATTACHMENT_BYTES as u64
                || descriptor.byte_len() != file.byte_len
                || descriptor.sha256() != &file.sha256
                || descriptors
                    .insert(*descriptor.id(), *descriptor)
                    .is_some_and(|old| old != *descriptor)
            {
                return Err(QueueCaptureFailure::Invalid);
            }
        }
        let metadata = InputMetadata {
            version: 1,
            generation: self.generation,
            revision: self.revision,
            source_sha256: self.source_sha256,
            files: self
                .files
                .iter()
                .map(|file| QueueNativeFileDescriptor {
                    id: file.id().as_str().to_owned(),
                    byte_len: file.byte_len(),
                    sha256: *file.sha256(),
                })
                .collect(),
            capture_len: self.capture.bytes().len() as u64,
            capture_sha256: self.capture.sha256(),
        };
        let bytes = serde_json::to_vec(&metadata).map_err(|_| QueueCaptureFailure::Invalid)?;
        envelope_len(bytes.len(), self.capture.bytes().len())?;
        Ok(bytes)
    }
}

fn envelope_len(metadata_len: usize, capture_len: usize) -> Result<usize, QueueCaptureFailure> {
    let length = PREFIX_BYTES
        .checked_add(metadata_len)
        .and_then(|length| length.checked_add(capture_len))
        .ok_or(QueueCaptureFailure::Invalid)?;
    if metadata_len == 0
        || metadata_len > QUEUE_BODY_BYTES
        || capture_len == 0
        || length as u64 > QUEUE_CAPTURE_BYTES
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(length)
}

fn validate_object(
    input: &PreparedComposerInput,
    operation: &QueueOperation,
    object: &QueueObject,
    object_id: &QueueObjectId,
    byte_len: u64,
    digest: &[u8; 32],
) -> Result<(), QueueCaptureFailure> {
    if object.id() != object_id
        || object.operation() != operation.id()
        || object.kind() != QueueObjectKind::NativeCompanion
        || !object.verified()
        || object.verification().generation != input.generation
        || object.reference().total_bytes() != byte_len
        || object.reference().sha256() != digest
    {
        return Err(QueueCaptureFailure::Invalid);
    }
    Ok(())
}

struct ReadInput {
    input: PreparedComposerInput,
    byte_len: u64,
    sha256: [u8; 32],
}

async fn read_saved(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    object: &QueueObjectId,
) -> Result<ReadInput, QueueCaptureFailure> {
    let directory = this
        .update_in(cx, |workspace, _, _| {
            let operation = workspace
                .queue_storage
                .operation(id)
                .ok_or(QueueCaptureFailure::Invalid)?;
            let preparation = operation
                .composer_preparation()
                .ok_or(QueueCaptureFailure::Invalid)?;
            let saved = workspace
                .queue_storage
                .object(object)
                .ok_or(QueueCaptureFailure::Invalid)?;
            if saved.operation() != id
                || saved.kind() != QueueObjectKind::NativeCompanion
                || !saved.verified()
                || saved.verification().generation != preparation.generation
                || saved.reference().total_bytes() > QUEUE_CAPTURE_BYTES
            {
                return Err(QueueCaptureFailure::Invalid);
            }
            Ok(operation.owner().directory().to_owned())
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    let bytes = queue_payload_io::read(this, cx, object).await?;
    cx.background_executor()
        .spawn(async move {
            let byte_len = bytes.len() as u64;
            let digest = sha256(&bytes);
            let input = PreparedComposerInput::decode(&bytes, &directory)?;
            Ok(ReadInput {
                input,
                byte_len,
                sha256: digest,
            })
        })
        .await
}

/// Persists and rereads the exact input through the committed worker before the
/// common ledger seals it. A prior seal accepts only identical saved bytes.
pub(crate) async fn persist_and_seal(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    input: &PreparedComposerInput,
    explicit_retry: bool,
) -> Result<QueueObjectId, QueueCaptureFailure> {
    let bound = this
        .update_in(cx, |workspace, _, _| {
            if workspace.close_in_progress() || !workspace.persistence_ready() {
                return Err(QueueCaptureFailure::Unavailable);
            }
            input.validate(&workspace.queue_storage, id)?;
            Ok(workspace
                .queue_storage
                .operation(id)
                .and_then(|operation| operation.composer_preparation())
                .and_then(|preparation| preparation.inputs_object())
                .cloned())
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    // Scoped background work borrows the capture, avoiding a foreground copy of
    // the complete input just to hand encoding to another executor.
    let mut encoded = None;
    cx.background_executor()
        .scoped(|scope| {
            scope.spawn(async {
                encoded = Some(input.encode().map(|bytes| {
                    let digest = sha256(&bytes);
                    (bytes, digest)
                }));
            });
        })
        .await;
    let (bytes, expected_sha256) = encoded.ok_or(QueueCaptureFailure::Unavailable)??;
    let expected_len = bytes.len() as u64;
    let object = if let Some(object) = bound {
        drop(bytes);
        object
    } else if explicit_retry {
        queue_payload_io::persist_retry(
            this,
            cx,
            id,
            "inputs",
            QueueObjectKind::NativeCompanion,
            bytes,
            false,
        )
        .await?
    } else {
        queue_payload_io::persist(
            this,
            cx,
            id,
            "inputs",
            QueueObjectKind::NativeCompanion,
            bytes,
            false,
        )
        .await?
    };
    let saved = read_saved(this, cx, id, &object).await?;
    if saved.byte_len != expected_len || saved.sha256 != expected_sha256 {
        return Err(QueueCaptureFailure::Invalid);
    }
    this.update_in(cx, |workspace, _, _| {
        if workspace.close_in_progress() || !workspace.persistence_ready() {
            return Err(QueueCaptureFailure::Unavailable);
        }
        saved.input.validate(&workspace.queue_storage, id)?;
        workspace
            .change_queue_storage(|queue, workspace| {
                let contexts = queue.composer_context_ids(id)?;
                let roots = queue.composer_file_descriptors(id)?;
                queue.seal_composer_inputs(
                    id,
                    saved.input.generation,
                    saved.input.revision,
                    object.clone(),
                    &workspace.attachments,
                    &workspace.attachment_preparations,
                    |operation, _, verified| {
                        saved
                            .input
                            .validate_operation(operation, &contexts, &roots)
                            .map_err(|_| {
                                JournalError::Invalid(
                                    "prepared input differs from captured composer authority",
                                )
                            })?;
                        validate_object(
                            &saved.input,
                            operation,
                            verified,
                            &object,
                            saved.byte_len,
                            &saved.sha256,
                        )
                        .map_err(|_| {
                            JournalError::Invalid(
                                "prepared input differs from verified object bytes",
                            )
                        })
                    },
                    |candidate| workspace.validate_queue_candidate(candidate),
                )
            })
            .map_err(QueueCaptureFailure::from)
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    Ok(object)
}

/// Restores only the already verified one-time inputs binding. Hydration and
/// retry never invoke mention reads, conversion, expansion or current typing.
pub(crate) async fn restore(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
) -> Result<PreparedComposerInput, QueueCaptureFailure> {
    let object = this
        .update_in(cx, |workspace, _, _| {
            workspace
                .queue_storage
                .operation(id)
                .and_then(|operation| operation.composer_preparation())
                .and_then(|preparation| preparation.inputs_object())
                .cloned()
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)?
        .ok_or(QueueCaptureFailure::Invalid)?;
    let saved = read_saved(this, cx, id, &object).await?;
    this.update_in(cx, |workspace, _, _| {
        saved.input.validate(&workspace.queue_storage, id)?;
        let operation = workspace
            .queue_storage
            .operation(id)
            .ok_or(QueueCaptureFailure::Invalid)?;
        if operation
            .composer_preparation()
            .and_then(|preparation| preparation.inputs_object())
            != Some(&object)
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        let verified = workspace
            .queue_storage
            .object(&object)
            .ok_or(QueueCaptureFailure::Invalid)?;
        validate_object(
            &saved.input,
            operation,
            verified,
            &object,
            saved.byte_len,
            &saved.sha256,
        )
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    Ok(saved.input)
}

#[cfg(test)]
#[path = "composer_payload_tests.rs"]
mod tests;
