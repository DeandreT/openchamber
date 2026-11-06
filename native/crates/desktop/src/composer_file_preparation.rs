//! Explicit server-file mention intake under an admitted composer operation.
//!
//! The caller owns parsing, the complete request list, append-only plan admission
//! and prepared-input sealing. This adapter reads only a new typed request. It
//! never obtains a path from existing attachment metadata or changes a draft.
//! A document's complete output plan and holds precede its first put. Publication
//! uses the queue ledger's receipt transition and saves each successful output.

use std::fmt;
use std::sync::Arc;

use gpui_kit::{AsyncWindowContext, WeakEntity};
use openchamber_native_core::draft::DraftKey;
use openchamber_native_core::ids::AttachmentId;
use openchamber_native_transport::attachment::AttachmentUpload;
use openchamber_native_transport::files::FilesError;
use openchamber_native_transport::queue_manifest::{
    QueueLocalCapture, QueueLocalFile, QueueMention,
};

use crate::attachment_catalog::{self, AttachmentCatalog};
use crate::attachment_files::{MAX_INLINE_ATTACHMENT_BYTES, NormalizedAttachmentFile};
use crate::attachment_intake::{
    self, AttachmentInputSource, AttachmentIntake, AttachmentIntakeError,
};
use crate::attachment_spool::{BlobDescriptor, BlobId};
use crate::mutation_journal::{self, JournalError, MAX_ENTRIES};
use crate::network::NetworkTask;
use crate::queue_capture::{QUEUE_CAPTURE_BYTES, QueueOperationId, sha256};
use crate::queue_payload_io::{self, QueueCaptureFailure};
use crate::queue_storage::{
    QueueAction, QueueComposerPreparation, QueueDerivedFile, QueueDerivedFileState,
    QueueEnqueuePhase, QueueOperation, QueuePrefixPhase, QueuePreparationMention,
    QueuePreparationSource, QueuePreparationSourceId, QueueSendPhase,
};
use crate::queue_take::QueueServer;
use crate::workspace::Workspace;

/// Failures contain classifications only, including when a server refused a read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileMentionPreparationError {
    Unavailable,
    WrongOwner,
    MissingAuthority,
    InvalidMention,
    ReferenceOnly,
    FrozenInputs,
    CaptureConflict,
    SourceChanged,
    InputTooLarge,
    Read,
    Intake(AttachmentIntakeError),
    Persistence,
    Put,
}

impl From<QueueCaptureFailure> for FileMentionPreparationError {
    fn from(error: QueueCaptureFailure) -> Self {
        match error {
            QueueCaptureFailure::Unavailable => Self::Unavailable,
            QueueCaptureFailure::Persistence => Self::Persistence,
            QueueCaptureFailure::Invalid => Self::CaptureConflict,
            QueueCaptureFailure::Transport => Self::Read,
        }
    }
}

impl From<JournalError> for FileMentionPreparationError {
    fn from(_: JournalError) -> Self {
        Self::CaptureConflict
    }
}

/// One shared pre-expansion source. Construct on the background executor once
/// per source, then share it across all of that source's mention requests.
pub(crate) struct CapturedFileMentionSource {
    owner: DraftKey,
    source_sha256: [u8; 32],
    generation: u64,
    revision: u64,
    source: QueuePreparationSource,
    capture: Arc<QueueLocalCapture>,
    boundaries: Vec<(u64, usize)>,
}

impl CapturedFileMentionSource {
    pub(crate) fn new(
        plan: &QueueComposerPreparation,
        identity: QueuePreparationSourceId,
        capture: Arc<QueueLocalCapture>,
    ) -> Result<Self, FileMentionPreparationError> {
        let source = plan
            .sources
            .iter()
            .find(|source| source.identity == identity)
            .ok_or(FileMentionPreparationError::MissingAuthority)?;
        let owner = plan.owner()?;
        if capture.bytes().is_empty()
            || capture.bytes().len() as u64 > QUEUE_CAPTURE_BYTES
            || source.byte_len != capture.bytes().len() as u64
            || source.sha256 != capture.sha256()
            || attachment_catalog::remote_path(capture.directory()).as_deref()
                != Some(owner.directory())
            || plan.generation == 0
            || plan.generation == u64::MAX
            || plan.revision == 0
            || plan.revision == u64::MAX
        {
            return Err(FileMentionPreparationError::CaptureConflict);
        }
        if plan.inputs_object().is_some() {
            return Err(FileMentionPreparationError::FrozenInputs);
        }
        // Sparse UTF-16 checkpoints bound each mention lookup without retaining
        // a second full text or rescanning a large prefix for every mention.
        let mut boundaries = vec![(0, 0)];
        let mut units = 0u64;
        for (byte, character) in capture.item().content.char_indices() {
            if units - boundaries.last().expect("the zero checkpoint exists").0 >= 4096 {
                boundaries.push((units, byte));
            }
            units += character.len_utf16() as u64;
        }
        Ok(Self {
            owner,
            source_sha256: plan.source_sha256,
            generation: plan.generation,
            revision: plan.revision,
            source: source.clone(),
            capture,
            boundaries,
        })
    }

    fn byte_offset(&self, offset: u64) -> Option<usize> {
        let index = self
            .boundaries
            .partition_point(|(units, _)| *units <= offset)
            .checked_sub(1)?;
        let (mut units, start) = self.boundaries[index];
        let text = &self.capture.item().content;
        for (byte, character) in text[start..].char_indices() {
            if units == offset {
                return Some(start + byte);
            }
            units = units.checked_add(character.len_utf16() as u64)?;
            if units > offset {
                return None;
            }
        }
        (units == offset).then_some(text.len())
    }

    fn validates(&self, mention: &QueueMention) -> bool {
        if mention.start >= mention.end || !mention.text.starts_with('@') {
            return false;
        }
        let Some(start) = self.byte_offset(mention.start) else {
            return false;
        };
        let Some(end) = self.byte_offset(mention.end) else {
            return false;
        };
        let text = &self.capture.item().content;
        text.get(start..end) == Some(mention.text.as_str())
            && text[..start].chars().next_back().is_none_or(|character| {
                mention_whitespace(character) || "()[]{}<>\"'`,.;:".contains(character)
            })
            && text[end..]
                .chars()
                .take_while(|character| !mention_whitespace(*character))
                .all(|character| ")]},.;:!?`\"'>".contains(character))
    }
}

fn mention_whitespace(character: char) -> bool {
    matches!(character, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

impl fmt::Debug for CapturedFileMentionSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CapturedFileMentionSource([redacted])")
    }
}

/// Only the typed mention parser may supply `resolved_path`. The server applies
/// its directory-scoped path authorization; this adapter requests no override.
#[derive(Clone)]
pub(crate) struct CapturedFileMention {
    source: Arc<CapturedFileMentionSource>,
    mention: QueuePreparationMention,
    server_path: String,
    filename: String,
}

impl CapturedFileMention {
    pub(crate) fn new(
        source: Arc<CapturedFileMentionSource>,
        mention: QueueMention,
        resolved_path: &str,
    ) -> Result<Self, FileMentionPreparationError> {
        if !source.validates(&mention) {
            return Err(FileMentionPreparationError::InvalidMention);
        }
        let server_path = attachment_catalog::remote_path(resolved_path)
            .ok_or(FileMentionPreparationError::InvalidMention)?;
        let filename = server_path
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .ok_or(FileMentionPreparationError::InvalidMention)?
            .to_owned();
        if !reads_on_send(&filename) {
            return Err(FileMentionPreparationError::ReferenceOnly);
        }
        AttachmentUpload::new(&filename, Vec::new())
            .map_err(|_| FileMentionPreparationError::InvalidMention)?;
        let typed_path = mention
            .text
            .strip_prefix('@')
            .expect("validated mention starts with @")
            .trim_start_matches(['`', '"', '\'', '<', '(', '[', '{'])
            .trim_end_matches([
                ')', ']', '}', ',', '.', ';', ':', '!', '?', '`', '"', '\'', '>',
            ]);
        if typed_path.rsplit(['/', '\\']).next() != Some(filename.as_str()) {
            return Err(FileMentionPreparationError::InvalidMention);
        }
        Ok(Self {
            source,
            mention: QueuePreparationMention {
                start: mention.start,
                end: mention.end,
                text: mention.text,
            },
            server_path,
            filename,
        })
    }
}

impl fmt::Debug for CapturedFileMention {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CapturedFileMention([redacted])")
    }
}

/// PDF and every other extension stay on the caller's pure reference path.
pub(crate) fn reads_on_send(filename: &str) -> bool {
    filename.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension.to_ascii_lowercase().as_str(),
            "docx"
                | "pptx"
                | "xlsx"
                | "odt"
                | "odp"
                | "ods"
                | "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "heic"
                | "heif"
        )
    })
}

fn is_document(filename: &str) -> bool {
    filename.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension.to_ascii_lowercase().as_str(),
            "docx" | "pptx" | "xlsx" | "odt" | "odp" | "ods"
        )
    })
}

struct NormalizedOutput {
    file: NormalizedAttachmentFile,
    sha256: [u8; 32],
}

/// Complete document text and cited images in intake order. Parent admission
/// records `plan`'s entire vector and holds before passing this to `prepare`.
pub(crate) struct NormalizedFileMention {
    request: CapturedFileMention,
    raw_source_byte_len: u64,
    raw_source_sha256: [u8; 32],
    outputs: Vec<NormalizedOutput>,
}

impl NormalizedFileMention {
    pub(crate) fn output_count(&self) -> usize {
        self.outputs.len()
    }
    pub(crate) fn raw_source_byte_len(&self) -> u64 {
        self.raw_source_byte_len
    }
    pub(crate) fn raw_source_sha256(&self) -> &[u8; 32] {
        &self.raw_source_sha256
    }
    pub(crate) fn document_group_required(&self) -> bool {
        is_document(&self.request.filename)
    }

    /// The parent supplies fresh local labels and a separate document-group ID.
    /// No labels, bytes, holds or catalogs are published by this pure conversion.
    pub(crate) fn plan(
        &self,
        labels: &[BlobId],
        document_group: Option<String>,
    ) -> Result<Vec<QueueDerivedFile>, FileMentionPreparationError> {
        if labels.len() != self.output_count()
            || labels.is_empty()
            || labels.len() > MAX_ENTRIES
            || document_group.is_some() != self.document_group_required()
            || labels
                .iter()
                .enumerate()
                .any(|(index, label)| labels[..index].contains(label))
        {
            return Err(FileMentionPreparationError::CaptureConflict);
        }
        if let Some(group) = &document_group {
            mutation_journal::identifier(group)?;
            if labels.iter().any(|label| label.as_str() == group) {
                return Err(FileMentionPreparationError::CaptureConflict);
            }
        }
        Ok(self
            .outputs
            .iter()
            .zip(labels)
            .enumerate()
            .map(|(output_ordinal, (output, blob))| QueueDerivedFile {
                source: self.request.source.source.identity,
                mention: self.request.mention.clone(),
                server_path: self.request.server_path.clone(),
                output_ordinal,
                local_attachment_id: blob.as_str().to_owned(),
                blob: blob.as_str().to_owned(),
                preparation_generation: self.request.source.generation,
                raw_source_byte_len: self.raw_source_byte_len(),
                raw_source_sha256: *self.raw_source_sha256(),
                byte_len: output.file.content.len() as u64,
                sha256: output.sha256,
                name: output.file.filename.clone(),
                mime_type: output.file.media_type.to_owned(),
                document_group: document_group.clone(),
                state: QueueDerivedFileState::Proposed,
            })
            .collect())
    }
}

impl fmt::Debug for NormalizedFileMention {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NormalizedFileMention")
            .field("outputs", &self.outputs.len())
            .finish_non_exhaustive()
    }
}

/// The descriptor comes from an exact catalog receipt. Provenance stays separate
/// from model-facing metadata and grants no authority for another source read.
pub(crate) struct PreparedFileMentionOutput {
    pub(crate) blob: BlobDescriptor,
    pub(crate) file: QueueLocalFile,
    pub(crate) provenance: QueueDerivedFile,
}

pub(crate) struct PreparedFileMention {
    pub(crate) owner: DraftKey,
    pub(crate) outputs: Vec<PreparedFileMentionOutput>,
}

impl fmt::Debug for PreparedFileMention {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedFileMention")
            .field("outputs", &self.outputs.len())
            .finish_non_exhaustive()
    }
}

fn validate_scope(
    operation: &QueueOperation,
    server: &QueueServer,
    request: &CapturedFileMention,
) -> Result<(), FileMentionPreparationError> {
    let source = &request.source;
    if source.owner.runtime() != &server.runtime
        || attachment_catalog::remote_path(server.scope.directory()).as_deref()
            != Some(source.owner.directory())
        || operation.owner().runtime() != server.runtime.as_str()
        || operation.owner().directory() != server.scope.directory()
        || operation.owner().session() != server.scope.session().as_str()
        || operation.owner().draft_key()? != source.owner
    {
        return Err(FileMentionPreparationError::WrongOwner);
    }
    let plan = operation
        .composer_preparation()
        .ok_or(FileMentionPreparationError::MissingAuthority)?;
    if plan.inputs_object().is_some() {
        return Err(FileMentionPreparationError::FrozenInputs);
    }
    if !matches!(
        operation.action(),
        QueueAction::Send {
            phase: QueueSendPhase::Capturing,
            ..
        } | QueueAction::Prefix {
            phase: QueuePrefixPhase::Capturing,
            ..
        } | QueueAction::Enqueue {
            phase: QueueEnqueuePhase::Capturing,
            ..
        }
    ) {
        return Err(FileMentionPreparationError::FrozenInputs);
    }
    if operation.source_sha256() != Some(&source.source_sha256)
        || plan.source_sha256 != source.source_sha256
        || plan.owner()? != source.owner
        || plan.generation != source.generation
        || plan.revision != source.revision
        || !plan.sources.contains(&source.source)
    {
        return Err(FileMentionPreparationError::CaptureConflict);
    }
    Ok(())
}

fn same_mention(file: &QueueDerivedFile, request: &CapturedFileMention) -> bool {
    file.source == request.source.source.identity && file.mention == request.mention
}

/// Indices are global plan positions. Output ordinals remain bundle-local.
fn bundle(
    plan: &QueueComposerPreparation,
    request: &CapturedFileMention,
) -> Result<Vec<(usize, QueueDerivedFile)>, FileMentionPreparationError> {
    let files: Vec<_> = plan
        .files
        .iter()
        .enumerate()
        .filter(|(_, file)| same_mention(file, request))
        .map(|(index, file)| (index, file.clone()))
        .collect();
    for (ordinal, (_, file)) in files.iter().enumerate() {
        if file.server_path != request.server_path
            || file.output_ordinal != ordinal
            || file.preparation_generation != request.source.generation
            || file.raw_source_byte_len > MAX_INLINE_ATTACHMENT_BYTES as u64
            || file.raw_source_byte_len != files[0].1.raw_source_byte_len
            || file.raw_source_sha256 != files[0].1.raw_source_sha256
            || file.document_group != files[0].1.document_group
            || file.document_group.is_some() != is_document(&request.filename)
        {
            return Err(FileMentionPreparationError::CaptureConflict);
        }
        file.descriptor()?;
    }
    Ok(files)
}

fn current_plan<'a>(
    workspace: &'a Workspace,
    id: &QueueOperationId,
    server: &QueueServer,
    request: &CapturedFileMention,
) -> Result<&'a QueueComposerPreparation, FileMentionPreparationError> {
    if workspace.close_in_progress() || !workspace.persistence_ready() {
        return Err(FileMentionPreparationError::Unavailable);
    }
    let operation = workspace
        .queue_storage
        .operation(id)
        .ok_or(FileMentionPreparationError::MissingAuthority)?;
    validate_scope(operation, server, request)?;
    Ok(operation
        .composer_preparation()
        .expect("scope checked preparation"))
}

fn current_bundle(
    workspace: &Workspace,
    id: &QueueOperationId,
    server: &QueueServer,
    request: &CapturedFileMention,
) -> Result<Vec<(usize, QueueDerivedFile)>, FileMentionPreparationError> {
    bundle(current_plan(workspace, id, server, request)?, request)
}

/// Only call for a mention without an admitted output bundle. Admission of the
/// original operation and its source plan must precede this first remote read.
pub(crate) async fn read_and_normalize(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    request: CapturedFileMention,
    reserved_filenames: Vec<String>,
) -> Result<NormalizedFileMention, FileMentionPreparationError> {
    let files = this
        .update_in(cx, |workspace, _, _| {
            current_bundle(workspace, id, server, &request)
        })
        .map_err(|_| FileMentionPreparationError::Unavailable)??;
    if !files.is_empty() {
        return Err(FileMentionPreparationError::CaptureConflict);
    }
    queue_payload_io::save(this, cx).await?;
    normalize_request(this, cx, id, server, request, reserved_filenames, None).await
}

async fn normalize_request(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    request: CapturedFileMention,
    reserved_filenames: Vec<String>,
    expected_raw: Option<(u64, [u8; 32])>,
) -> Result<NormalizedFileMention, FileMentionPreparationError> {
    this.update_in(cx, |workspace, _, _| {
        current_plan(workspace, id, server, &request).map(|_| ())
    })
    .map_err(|_| FileMentionPreparationError::Unavailable)??;
    let client = server.client.clone();
    let directory = server.scope.directory().to_owned();
    let path = request.server_path.clone();
    let read = NetworkTask::spawn_on(&server.executor, async move {
        client
            .read_raw_file(&directory, &path, MAX_INLINE_ATTACHMENT_BYTES)
            .await
            .map_err(|error| match error {
                FilesError::RawFileTooLarge => FileMentionPreparationError::InputTooLarge,
                _ => FileMentionPreparationError::Read,
            })
    })
    .await
    .ok_or(FileMentionPreparationError::Unavailable)??;
    this.update_in(cx, |workspace, _, _| {
        current_plan(workspace, id, server, &request).map(|_| ())
    })
    .map_err(|_| FileMentionPreparationError::Unavailable)??;
    // Header MIME is deliberately discarded. Intake selects conversion from the
    // explicit filename and validates the actual bytes on its existing boundary.
    cx.background_executor()
        .spawn(async move {
            normalize_bytes(request, read.into_bytes(), reserved_filenames, expected_raw)
        })
        .await
}

fn normalize_bytes(
    request: CapturedFileMention,
    bytes: Vec<u8>,
    reserved_filenames: Vec<String>,
    expected_raw: Option<(u64, [u8; 32])>,
) -> Result<NormalizedFileMention, FileMentionPreparationError> {
    if bytes.len() > MAX_INLINE_ATTACHMENT_BYTES {
        return Err(FileMentionPreparationError::InputTooLarge);
    }
    let raw_source_byte_len = bytes.len() as u64;
    let raw_source_sha256 = sha256(&bytes);
    if expected_raw.is_some_and(|expected| expected != (raw_source_byte_len, raw_source_sha256)) {
        return Err(FileMentionPreparationError::SourceChanged);
    }
    let outputs = attachment_intake::prepare_attachment(AttachmentIntake {
        source: AttachmentInputSource::Bytes(bytes),
        filename: request.filename.clone(),
        declared_media_type: None,
        reserved_filenames,
    })
    .map_err(FileMentionPreparationError::Intake)?;
    if outputs.is_empty() || outputs.len() > MAX_ENTRIES {
        return Err(FileMentionPreparationError::CaptureConflict);
    }
    let outputs = outputs
        .into_iter()
        .map(|file| {
            let sha256 = sha256(&file.content);
            NormalizedOutput { file, sha256 }
        })
        .collect();
    Ok(NormalizedFileMention {
        request,
        raw_source_byte_len,
        raw_source_sha256,
        outputs,
    })
}

fn match_outputs(
    normalized: &NormalizedFileMention,
    saved: &[(usize, QueueDerivedFile)],
) -> Result<(), FileMentionPreparationError> {
    if normalized.outputs.len() != saved.len() || saved.is_empty() {
        return Err(FileMentionPreparationError::SourceChanged);
    }
    for (output, (_, planned)) in normalized.outputs.iter().zip(saved) {
        if planned.raw_source_byte_len != normalized.raw_source_byte_len
            || planned.raw_source_sha256 != normalized.raw_source_sha256
            || planned.byte_len != output.file.content.len() as u64
            || planned.sha256 != output.sha256
            || planned.name != output.file.filename
            || planned.mime_type != output.file.media_type
        {
            return Err(FileMentionPreparationError::SourceChanged);
        }
    }
    Ok(())
}

/// Consumes only the exact complete bundle the parent has append-admitted.
/// Failure retains every successful catalog publication and all remaining holds.
/// The parent must report incomplete and must not seal or dispatch partial inputs.
pub(crate) async fn prepare(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    normalized: NormalizedFileMention,
) -> Result<PreparedFileMention, FileMentionPreparationError> {
    publish_bundle(this, cx, id, server, normalized, false).await
}

/// Explicit activation only. Complete bundles return held catalog bindings
/// without a remote read. A partial bundle rereads its source once and compares
/// raw identity and every output, including already cataloged siblings, before
/// any resume. Reserved names must exclude this bundle's own output names.
pub(crate) async fn retry(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    request: CapturedFileMention,
    reserved_filenames: Vec<String>,
) -> Result<PreparedFileMention, FileMentionPreparationError> {
    let saved = this
        .update_in(cx, |workspace, _, _| {
            current_bundle(workspace, id, server, &request)
        })
        .map_err(|_| FileMentionPreparationError::Unavailable)??;
    if saved.is_empty() {
        return Err(FileMentionPreparationError::MissingAuthority);
    }
    queue_payload_io::save(this, cx).await?;
    if saved
        .iter()
        .all(|(_, file)| file.state == QueueDerivedFileState::Cataloged)
    {
        return finish(this, cx, id, server, &request);
    }
    let expected_raw = Some((saved[0].1.raw_source_byte_len, saved[0].1.raw_source_sha256));
    let normalized = normalize_request(
        this,
        cx,
        id,
        server,
        request,
        reserved_filenames,
        expected_raw,
    )
    .await?;
    publish_bundle(this, cx, id, server, normalized, true).await
}

async fn publish_bundle(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    normalized: NormalizedFileMention,
    explicit_retry: bool,
) -> Result<PreparedFileMention, FileMentionPreparationError> {
    let saved = this
        .update_in(cx, |workspace, _, _| {
            current_bundle(workspace, id, server, &normalized.request)
        })
        .map_err(|_| FileMentionPreparationError::Unavailable)??;
    match_outputs(&normalized, &saved)?;
    // One barrier commits the entire document plan and all pre-put holds.
    queue_payload_io::save(this, cx).await?;
    let NormalizedFileMention {
        request, outputs, ..
    } = normalized;
    for ((ordinal, planned), output) in saved.into_iter().zip(outputs) {
        let descriptor = planned.descriptor()?;
        let put = this
            .update_in(cx, |workspace, _, _| {
                let file = current_plan(workspace, id, server, &request)?
                    .files
                    .get(ordinal)
                    .ok_or(FileMentionPreparationError::CaptureConflict)?;
                if !same_output_plan(file, &planned) {
                    return Err(FileMentionPreparationError::CaptureConflict);
                }
                if file.state == QueueDerivedFileState::Cataloged {
                    cataloged_descriptor(&workspace.attachments, file)?;
                    return Ok(None);
                }
                let hold = file.hold(&request.source.owner)?;
                if !workspace.attachment_preparations.contains(&hold) {
                    return Err(FileMentionPreparationError::MissingAuthority);
                }
                let worker = workspace
                    .attachment_worker()
                    .ok_or(FileMentionPreparationError::Unavailable)?;
                Ok(Some(if explicit_retry {
                    worker.resume_attachment(hold, descriptor, output.file.content)
                } else {
                    worker.put_attachment(
                        hold.owner,
                        hold.id,
                        hold.generation,
                        hold.blob,
                        output.file.content,
                    )
                }))
            })
            .map_err(|_| FileMentionPreparationError::Unavailable)??;
        let Some(put) = put else {
            continue;
        };
        let receipt = put
            .await
            .map_err(|_| FileMentionPreparationError::Persistence)?
            .map_err(|_| FileMentionPreparationError::Put)?;
        this.update_in(cx, |workspace, _, _| {
            current_plan(workspace, id, server, &request)?;
            publish_receipt(workspace, id, request.source.generation, ordinal, &receipt)
        })
        .map_err(|_| FileMentionPreparationError::Unavailable)??;
        queue_payload_io::save(this, cx).await?;
    }
    finish(this, cx, id, server, &request)
}

fn same_output_plan(left: &QueueDerivedFile, right: &QueueDerivedFile) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.state = QueueDerivedFileState::Proposed;
    right.state = QueueDerivedFileState::Proposed;
    left == right
}

fn cataloged_descriptor(
    catalog: &AttachmentCatalog,
    file: &QueueDerivedFile,
) -> Result<BlobDescriptor, FileMentionPreparationError> {
    let descriptor = file.descriptor()?;
    let attachment = catalog
        .get(&AttachmentId::from(file.local_attachment_id.clone()))
        .ok_or(FileMentionPreparationError::MissingAuthority)?;
    if file.state != QueueDerivedFileState::Cataloged
        || attachment.blob != descriptor
        || attachment
            .staging
            .as_ref()
            .is_some_and(|staging| staging.filename != file.name)
    {
        return Err(FileMentionPreparationError::CaptureConflict);
    }
    Ok(attachment.blob)
}

fn finish(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    id: &QueueOperationId,
    server: &QueueServer,
    request: &CapturedFileMention,
) -> Result<PreparedFileMention, FileMentionPreparationError> {
    this.update_in(cx, |workspace, _, _| {
        let saved = current_bundle(workspace, id, server, request)?;
        if saved.is_empty() {
            return Err(FileMentionPreparationError::MissingAuthority);
        }
        let outputs = saved
            .into_iter()
            .map(|(_, provenance)| prepared_output(&workspace.attachments, request, provenance))
            .collect::<Result<Vec<_>, FileMentionPreparationError>>()?;
        Ok(PreparedFileMention {
            owner: request.source.owner.clone(),
            outputs,
        })
    })
    .map_err(|_| FileMentionPreparationError::Unavailable)?
}

fn prepared_output(
    catalog: &AttachmentCatalog,
    request: &CapturedFileMention,
    provenance: QueueDerivedFile,
) -> Result<PreparedFileMentionOutput, FileMentionPreparationError> {
    let blob = cataloged_descriptor(catalog, &provenance)?;
    let file = QueueLocalFile {
        name: provenance.name.clone(),
        mime_type: provenance.mime_type.clone(),
        byte_len: blob.byte_len(),
        sha256: *blob.sha256(),
        description: None,
        mention: (provenance.output_ordinal == 0).then(|| QueueMention {
            start: request.mention.start,
            end: request.mention.end,
            text: request.mention.text.clone(),
        }),
    };
    Ok(PreparedFileMentionOutput {
        blob,
        file,
        provenance,
    })
}

fn publish_receipt(
    workspace: &mut Workspace,
    id: &QueueOperationId,
    generation: u64,
    ordinal: usize,
    receipt: &crate::attachment_spool::BlobReceipt,
) -> Result<(), FileMentionPreparationError> {
    // Move the three coupled collections together. The existing canonical
    // validator sees the complete candidate, including unrelated live owners.
    let mut queue = std::mem::take(&mut workspace.queue_storage);
    let mut attachments = std::mem::take(&mut workspace.attachments);
    let mut holds = std::mem::take(&mut workspace.attachment_preparations);
    let result = queue.publish_composer_derived_file(
        id,
        generation,
        ordinal,
        receipt,
        &mut attachments,
        &mut holds,
        |candidate, catalog, preparations| {
            workspace.attachments = catalog.clone();
            workspace.attachment_preparations = preparations.to_vec();
            // The validator takes the candidate ledger explicitly. Do not clone
            // inline original sources just to expose the candidate collections.
            workspace.validate_queue_candidate(candidate)
        },
    );
    workspace.queue_storage = queue;
    workspace.attachments = attachments;
    workspace.attachment_preparations = holds;
    result.map_err(Into::into)
}

#[cfg(test)]
#[path = "composer_file_preparation_tests.rs"]
mod tests;
