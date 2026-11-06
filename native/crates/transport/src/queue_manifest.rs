//! Opt-in durable queue requests and individually uploaded attachment bytes.
//!
//! Captures retain one client, exact directory, immutable body and caller-chosen
//! request identity. Persist them and the local spool before submitting. A lost
//! answer never triggers a retry, a new identity, acknowledgement or deletion.
//! Prefix sends use preview followed by reservation; take is an edit/handoff.
//!
//! This crate has no SHA-256 implementation. Upload digests come from the caller.
//! Downloads validate bounds, length and the advertised digest, but expose
//! explicitly unverified bytes which the caller must hash before journaling or
//! acknowledging a transfer. Payload APIs require the destination's SHA-256
//! callback to verify index bindings and complete capture/chunk bytes. They keep
//! 50 MiB active captures separate from compact 4 MiB requests and receipts.
//! Queue skills retain mentions which core SkillRef
//! cannot currently represent. No conversion silently discards those mentions.
//!
//! Captured integer metadata is limited to JavaScript's exact safe-integer range.
//! Finite f64 metadata already represents a JavaScript number and is retained.
//! JavaScript replies project metadata numbers to f64; queue content equality
//! compares their numeric values recursively, including 1.0 == 1 and -0 == 0.
//! This does not change retained request bytes or compute a local request hash.

use std::collections::HashSet;
use std::fmt;
use std::io;

use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::message_queue::{QueuedItemId, QueuedSendConfig, queue_owner};
use reqwest::header::{CONTENT_TYPE, HeaderMap};
use reqwest::{Method, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, encode_directory};
use crate::endpoint::Endpoint;
use crate::prompt_parts::Metadata;
use crate::request::RequestBuilder;

/// Maximum encoded request or full receipt size, independently of queue growth.
pub const QUEUE_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
/// Maximum bytes in one prepared attachment.
pub const QUEUE_BLOB_BYTES: usize = 20 * 1024 * 1024;
/// Maximum combined files, agents, skills and expanded synthetic context.
pub const QUEUE_MANIFEST_ENTRIES: usize = 4096;
/// Maximum complete UTF-8 JSON capture, including directory and JSON syntax.
pub const QUEUE_PAYLOAD_BYTES: usize = 50 * 1024 * 1024;
/// Maximum actual encoded admission body. An oversized server index remains
/// editable; the destination must check its own staged-file request size.
pub const QUEUE_ADMISSION_BYTES: u64 = 128 * 1024 * 1024;
/// The destination supplies its existing SHA-256 implementation. This callback
/// hashes the exact supplied bytes, including schema-ordered index JSON.
pub type QueueSha256 = fn(&[u8]) -> [u8; 32];
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const ERROR_BYTES: usize = 4096;

/// Content-free failures. Server error text and request content are discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum QueueManifestError {
    #[error("invalid captured queue input")]
    InvalidInput,
    #[error("queue metadata exceeds its limit")]
    MetadataTooLarge,
    #[error("attachment exceeds its size limit")]
    BlobTooLarge,
    #[error("structured queue capture exceeds its size limit")]
    PayloadTooLarge,
    #[error("structured queue capture or index digest differs")]
    PayloadMismatch,
    #[error("queue response is malformed")]
    InvalidResponse,
    #[error("queue response belongs to another captured request")]
    ReceiptMismatch,
    #[error("queue request is unknown")]
    UnknownRequest,
    #[error("queue request conflicts with retained ownership")]
    Conflict,
    /// The queue changed while the session held no reservation, so this
    /// prefix request reserved nothing and never will.
    #[error("queue prefix changed")]
    PrefixStale,
    #[error("queue request persistence or upload is pending")]
    Pending,
    #[error("queue receipt history requires recovery")]
    RecoveryRequired,
    #[error("attachment response length or digest header differs")]
    AttachmentMismatch,
    #[error("the server requires authentication")]
    AuthenticationRequired,
    #[error("the server answered with a redirect, which is never followed")]
    Redirected,
    #[error("the queue request did not answer in time")]
    TimedOut,
    #[error("the queue request failed")]
    RequestFailed,
    #[error("the server answered HTTP {status}")]
    Http { status: u16 },
}

/// Captured runtime, real session and exact directory. Construction performs no I/O.
#[derive(Clone)]
pub struct QueueScope {
    client: ServerClient,
    session: SessionId,
    directory: String,
}

impl QueueScope {
    /// The caller binds `client` to the original verified runtime on recovery.
    /// Directory spelling is retained exactly, including remote Windows paths.
    pub fn new(
        client: &ServerClient,
        session: SessionId,
        directory: &str,
    ) -> Result<Self, QueueManifestError> {
        if !ascii_id(session.as_str(), 4, 128)
            || directory.is_empty()
            || js_len(directory) > 16384
            || js_trim(directory) != directory
        {
            return Err(QueueManifestError::InvalidInput);
        }
        Ok(Self {
            client: client.clone(),
            session,
            directory: directory.to_owned(),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        self.client.endpoint()
    }
    pub fn session(&self) -> &SessionId {
        &self.session
    }
    pub fn directory(&self) -> &str {
        &self.directory
    }

    /// Retains a caller-created request ID. This never allocates a replacement ID.
    pub fn request(&self, request_id: &str) -> Result<QueueRequestOwner, QueueManifestError> {
        if !ascii_id(request_id, 8, 128) {
            return Err(QueueManifestError::InvalidInput);
        }
        Ok(QueueRequestOwner {
            scope: self.clone(),
            request_id: request_id.to_owned(),
        })
    }

    fn url(&self, tail: &[&str]) -> Url {
        let mut url = self.client.endpoint().url("api/message-queue/sessions/");
        let mut segments = url.path_segments_mut().expect("HTTP URL has segments");
        segments.pop_if_empty().push(self.session.as_str());
        for segment in tail {
            segments.push(segment);
        }
        drop(segments);
        url
    }

    fn authorized(&self, method: Method, url: Url) -> RequestBuilder {
        let request = self.client.authorized(method, url);
        if queue_owner(&self.directory).is_some() {
            request
                .header("x-opencode-directory", encode_directory(&self.directory))
                .header("x-opencode-directory-encoding", "uri")
        } else {
            request
        }
    }
}

impl fmt::Debug for QueueScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueScope")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// A request identity cannot move between sessions, exact directories or runtimes.
#[derive(Clone)]
pub struct QueueRequestOwner {
    scope: QueueScope,
    request_id: String,
}

impl QueueRequestOwner {
    pub fn scope(&self) -> &QueueScope {
        &self.scope
    }
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    fn url(&self, tail: &[&str], directory_query: bool) -> Url {
        let mut segments = vec!["requests", self.request_id.as_str()];
        segments.extend_from_slice(tail);
        let mut url = self.scope.url(&segments);
        if directory_query {
            url.query_pairs_mut()
                .append_pair("directory", &self.scope.directory);
        }
        url
    }

    /// Exact bounded recovery. Only the declared 404 code means unknown.
    /// A previous receipt, when supplied, must match in full.
    pub async fn lookup(
        &self,
        expected: Option<&QueueReceipt>,
    ) -> Result<QueueLookup, QueueManifestError> {
        if let Some(receipt) = expected {
            validate_receipt_owner(receipt.identity(), self)?;
        }
        let request = self.scope.authorized(Method::GET, self.url(&[], true));
        let body = match json_body(send(request).await?).await {
            Err(QueueManifestError::UnknownRequest) => return Ok(QueueLookup::Unknown),
            result => result?,
        };
        let record = parse_record(&body, self)?;
        if expected.is_some_and(|expected| *expected != record.receipt()) {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        Ok(QueueLookup::Found(Box::new(record)))
    }
}

impl fmt::Debug for QueueRequestOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueRequestOwner")
            .field("scope", &self.scope)
            .field("request_id", &self.request_id)
            .finish()
    }
}

/// UTF-16 mention offsets from the queue boundary. Safe JS integers are retained
/// without narrowing to core's u32 range or losing skill mention text.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct QueueMention {
    pub start: u64,
    pub end: u64,
    pub text: String,
}

impl fmt::Debug for QueueMention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueMention")
            .field("start", &self.start)
            .field("end", &self.end)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct QueueAgent {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<QueueMention>,
}

impl fmt::Debug for QueueAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QueueAgent([redacted])")
    }
}

/// Unlike core SkillRef, this record retains the backend's optional mention.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct QueueSkill {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<QueueMention>,
}

impl fmt::Debug for QueueSkill {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QueueSkill([redacted])")
    }
}

/// An explicit server-file reference. It carries no prepared bytes, upload
/// identity or queue file proof, and transport never reads its URI.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct QueueFileReference {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<QueueMention>,
}

impl fmt::Debug for QueueFileReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QueueFileReference([redacted])")
    }
}

/// Captured synthetic context whose ID may be allocated by the server receipt.
#[derive(Clone, Eq, Serialize)]
pub struct QueuePromptContext {
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_message_id"
    )]
    pub id: Option<MessageId>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl PartialEq for QueuePromptContext {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.text == other.text
            && self.description == other.description
            && optional_metadata_equal(self.metadata.as_ref(), other.metadata.as_ref())
    }
}

impl fmt::Debug for QueuePromptContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuePromptContext")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Complete captured parts. No merge policy or conversion to a lossy core prompt
/// is implemented here; the destination adapter must preserve every field.
#[derive(Clone, Debug, Default, Eq, Serialize)]
pub struct QueuePromptParts {
    #[serde(rename = "fileReferences", skip_serializing_if = "Vec::is_empty")]
    pub file_references: Vec<QueueFileReference>,
    pub agents: Vec<QueueAgent>,
    pub skills: Vec<QueueSkill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub context: Vec<QueuePromptContext>,
}

fn serialize_optional_message_id<S: serde::Serializer>(
    id: &Option<MessageId>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    id.as_ref().map(MessageId::as_str).serialize(serializer)
}

impl PartialEq for QueuePromptParts {
    fn eq(&self, other: &Self) -> bool {
        self.file_references == other.file_references
            && self.agents == other.agents
            && self.skills == other.skills
            && self.context == other.context
            && optional_metadata_equal(self.metadata.as_ref(), other.metadata.as_ref())
    }
}

/// Existing queue context kinds remain distinct through a manifest handoff.
#[derive(Clone, Eq)]
pub enum QueueLegacyContext {
    Context {
        text: String,
        metadata: Metadata,
        instructions: Option<String>,
    },
    Instruction {
        text: String,
    },
    Synthetic {
        text: String,
    },
}

impl PartialEq for QueueLegacyContext {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Context {
                    text,
                    metadata,
                    instructions,
                },
                Self::Context {
                    text: other_text,
                    metadata: other_metadata,
                    instructions: other_instructions,
                },
            ) => {
                text == other_text
                    && instructions == other_instructions
                    && metadata_equal(metadata, other_metadata)
            }
            (Self::Instruction { text }, Self::Instruction { text: other_text })
            | (Self::Synthetic { text }, Self::Synthetic { text: other_text }) => {
                text == other_text
            }
            _ => false,
        }
    }
}

impl fmt::Debug for QueueLegacyContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QueueLegacyContext([redacted])")
    }
}

/// Immutable uploaded byte identity. Model-facing names, descriptions and
/// mentions belong to each use and live in QueueManifestAttachment instead.
#[derive(Clone, PartialEq, Eq)]
pub struct QueueAttachmentDescriptor {
    pub blob_id: [u8; 32],
    pub request_id: String,
    pub attachment_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

impl fmt::Debug for QueueAttachmentDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueAttachmentDescriptor")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct QueueManifestAttachment {
    pub attachment: QueueAttachmentDescriptor,
    /// Exact model-facing identity. This grants no path read or byte authority.
    pub agent_path: Option<String>,
    pub description: Option<String>,
    pub mention: Option<QueueMention>,
}

impl fmt::Debug for QueueManifestAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueManifestAttachment")
            .field("attachment", &self.attachment)
            .finish_non_exhaustive()
    }
}

/// Strict enqueue input, with no IDs, inline bytes, goal or scheduled provenance.
#[derive(Clone, PartialEq, Eq)]
pub struct QueueManifestItem {
    pub content: String,
    pub text: String,
    pub attachment_manifest: Vec<QueueManifestAttachment>,
    pub context: Vec<QueueLegacyContext>,
    pub context_preview: Option<PayloadPreview>,
    pub send_config: QueuedSendConfig,
    pub prompt_parts: QueuePromptParts,
}

impl fmt::Debug for QueueManifestItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueManifestItem")
            .field("files", &self.attachment_manifest.len())
            .field("parts", &self.prompt_parts)
            .finish_non_exhaustive()
    }
}

/// A full transferred item, parsed independently of any current queue snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueTakenItem {
    pub id: QueuedItemId,
    pub created_at: u64,
    pub request_id: Option<String>,
    pub item: QueueManifestItem,
}

/// Prepared raw upload. Retain the original digest and spool before submit.
pub struct QueueUpload {
    owner: QueueRequestOwner,
    attachment_id: String,
    filename: String,
    mime_type: String,
    sha256: [u8; 32],
    bytes: Vec<u8>,
}

impl QueueUpload {
    /// Whether a name is accepted as an upload basename. Model-facing identities
    /// belong to the manifest use and may have a different spelling.
    #[must_use]
    pub fn is_valid_filename(filename: &str) -> bool {
        valid_filename(filename)
    }

    /// SHA-256 must be computed over these exact prepared bytes by the caller.
    pub fn new(
        owner: QueueRequestOwner,
        attachment_id: &str,
        filename: &str,
        mime_type: &str,
        sha256: [u8; 32],
        bytes: Vec<u8>,
    ) -> Result<Self, QueueManifestError> {
        if bytes.len() > QUEUE_BLOB_BYTES {
            return Err(QueueManifestError::BlobTooLarge);
        }
        if !ascii_id(attachment_id, 1, 128) || !valid_filename(filename) || !valid_mime(mime_type) {
            return Err(QueueManifestError::InvalidInput);
        }
        Ok(Self {
            owner,
            attachment_id: attachment_id.to_owned(),
            filename: filename.to_owned(),
            mime_type: mime_type.to_owned(),
            sha256,
            bytes,
        })
    }

    pub fn owner(&self) -> &QueueRequestOwner {
        &self.owner
    }
    pub fn attachment_id(&self) -> &str {
        &self.attachment_id
    }
    pub fn filename(&self) -> &str {
        &self.filename
    }
    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn request(&self) -> RequestBuilder {
        self.owner
            .scope
            .authorized(
                Method::PUT,
                self.owner.url(&["attachments", &self.attachment_id], true),
            )
            .header(CONTENT_TYPE.as_str(), "application/octet-stream")
            .header("x-openchamber-filename", encode_directory(&self.filename))
            .header("x-openchamber-mime-type", &self.mime_type)
            .header("x-openchamber-size", self.bytes.len().to_string())
            .header("x-openchamber-sha256", hex_digest(&self.sha256))
            .body(self.bytes.clone())
    }

    /// One explicit raw PUT. Success validates all echoed immutable metadata.
    pub async fn submit(&self) -> Result<QueueAttachmentDescriptor, QueueManifestError> {
        let body = json_body(send(self.request()).await?).await?;
        let reply: AttachmentReplyWire = decode(&body)?;
        let attachment = descriptor(reply.attachment)?;
        if reply.state != "available"
            || attachment.request_id != self.owner.request_id
            || attachment.attachment_id != self.attachment_id
            || attachment.filename != self.filename
            || attachment.mime_type != self.mime_type
            || attachment.size != self.bytes.len() as u64
            || attachment.sha256 != self.sha256
        {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        Ok(attachment)
    }
}

impl fmt::Debug for QueueUpload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueUpload")
            .field("owner", &self.owner)
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// Access to a descriptor through its original upload identity and captured scope.
pub struct QueueAttachmentHandle {
    scope: QueueScope,
    descriptor: QueueAttachmentDescriptor,
}

impl QueueAttachmentHandle {
    pub fn new(
        scope: QueueScope,
        descriptor: QueueAttachmentDescriptor,
    ) -> Result<Self, QueueManifestError> {
        validate_descriptor(&descriptor)?;
        Ok(Self { scope, descriptor })
    }

    pub fn descriptor(&self) -> &QueueAttachmentDescriptor {
        &self.descriptor
    }

    fn request(&self, method: Method) -> RequestBuilder {
        let mut url = self.scope.url(&[
            "requests",
            &self.descriptor.request_id,
            "attachments",
            &self.descriptor.attachment_id,
        ]);
        url.query_pairs_mut()
            .append_pair("directory", &self.scope.directory);
        self.scope.authorized(method, url)
    }

    /// Exact-length, bounded bytes with matching advertised digest. The caller
    /// must hash the returned bytes itself before granting durable ownership.
    pub async fn download(&self) -> Result<UnverifiedQueueBytes, QueueManifestError> {
        let mut response = send(self.request(Method::GET)).await?;
        require_ok(&mut response).await?;
        if !content_type(response.headers(), "application/octet-stream") {
            return Err(QueueManifestError::InvalidResponse);
        }
        let digest = response
            .headers()
            .get("x-openchamber-sha256")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_digest)
            .ok_or(QueueManifestError::InvalidResponse)?;
        if response
            .content_length()
            .is_some_and(|n| n > QUEUE_BLOB_BYTES as u64)
        {
            return Err(QueueManifestError::BlobTooLarge);
        }
        if digest != self.descriptor.sha256
            || response
                .content_length()
                .is_some_and(|n| n != self.descriptor.size)
        {
            return Err(QueueManifestError::AttachmentMismatch);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(request_error)? {
            if chunk.len() > QUEUE_BLOB_BYTES - bytes.len() {
                return Err(QueueManifestError::BlobTooLarge);
            }
            if chunk.len() as u64 > self.descriptor.size - bytes.len() as u64 {
                return Err(QueueManifestError::AttachmentMismatch);
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() as u64 != self.descriptor.size {
            return Err(QueueManifestError::AttachmentMismatch);
        }
        Ok(UnverifiedQueueBytes {
            bytes,
            expected_sha256: digest,
        })
    }

    /// Explicit unreferenced release. No other operation calls this implicitly.
    pub async fn release(&self) -> Result<(), QueueManifestError> {
        let bytes = json_body(send(self.request(Method::DELETE)).await?).await?;
        let reply: AttachmentReplyWire = decode(&bytes)?;
        if reply.state != "released" || descriptor(reply.attachment)? != self.descriptor {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        Ok(())
    }
}

impl fmt::Debug for QueueAttachmentHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueAttachmentHandle")
            .field("scope", &self.scope)
            .field("descriptor", &self.descriptor)
            .finish()
    }
}

/// These bytes have NOT been hashed by transport. Comparing a server-provided
/// digest header cannot establish their integrity.
pub struct UnverifiedQueueBytes {
    bytes: Vec<u8>,
    expected_sha256: [u8; 32],
}

impl UnverifiedQueueBytes {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn expected_sha256(&self) -> [u8; 32] {
        self.expected_sha256
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl fmt::Debug for UnverifiedQueueBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnverifiedQueueBytes")
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// Captured full manifest. `body()` is the exact JSON to journal before POST.
pub struct QueueEnqueue {
    owner: QueueRequestOwner,
    item: QueueManifestItem,
    body: Vec<u8>,
}

impl QueueEnqueue {
    pub fn new(
        owner: QueueRequestOwner,
        item: QueueManifestItem,
    ) -> Result<Self, QueueManifestError> {
        validate_item(&item)?;
        let value = json!({"directory": owner.scope.directory, "item": item_value(&item)});
        capture_manifest_bound(&owner, &value, item.context_preview.as_ref())?;
        let body = encode_payload_capture(
            &owner.scope.directory,
            item_value(&item),
            item.context_preview.as_ref(),
            PayloadEncoding::Manifest,
        )?;
        Ok(Self { owner, item, body })
    }

    /// Restores the original immutable body without normalizing omitted fields.
    pub fn from_body(owner: QueueRequestOwner, body: Vec<u8>) -> Result<Self, QueueManifestError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Body {
            directory: String,
            item: PayloadCaptureItem,
        }
        let raw: Body = decode_input(&body)?;
        if raw.directory != owner.scope.directory {
            return Err(QueueManifestError::InvalidInput);
        }
        let mut wire: ItemWire = serde_json::from_value(Value::Object(raw.item.fields.clone()))
            .map_err(|_| QueueManifestError::InvalidInput)?;
        wire.context_preview = raw.item.context_preview;
        let item = input_item(wire).map_err(|_| QueueManifestError::InvalidInput)?;
        let value = json!({"directory":raw.directory,"item":raw.item.fields});
        capture_manifest_bound(&owner, &value, item.context_preview.as_ref())?;
        Ok(Self { owner, item, body })
    }

    pub fn owner(&self) -> &QueueRequestOwner {
        &self.owner
    }
    pub fn item(&self) -> &QueueManifestItem {
        &self.item
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub async fn submit(&self) -> Result<QueueEnqueueRecord, QueueManifestError> {
        let record = post_record(&self.owner, "enqueue", &self.body).await?;
        let QueueRecord::Enqueue(record) = record else {
            return Err(QueueManifestError::ReceiptMismatch);
        };
        validate_context_ids(&record.receipt.context_ids, &self.item)?;
        Ok(record)
    }

    /// Recovery still validates operation kind and captured context identities.
    pub async fn lookup(&self) -> Result<Option<QueueEnqueueRecord>, QueueManifestError> {
        match self.owner.lookup(None).await? {
            QueueLookup::Unknown => Ok(None),
            QueueLookup::Found(record) => match *record {
                QueueRecord::Enqueue(record) => {
                    validate_context_ids(&record.receipt.context_ids, &self.item)?;
                    Ok(Some(record))
                }
                _ => Err(QueueManifestError::ReceiptMismatch),
            },
        }
    }
}

impl fmt::Debug for QueueEnqueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueEnqueue")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

/// A manifest transfer keeps server ownership until explicit acknowledgement.
pub struct QueueTake {
    owner: QueueRequestOwner,
    item_ids: Vec<QueuedItemId>,
    body: Vec<u8>,
}

impl QueueTake {
    pub fn new(
        owner: QueueRequestOwner,
        item_ids: Vec<QueuedItemId>,
    ) -> Result<Self, QueueManifestError> {
        validate_item_ids(&item_ids)?;
        let value = json!({"directory": owner.scope.directory,
            "itemIds": item_ids.iter().map(QueuedItemId::as_str).collect::<Vec<_>>(), "format": "manifest-v1"});
        capture_bound("take", &owner, &value)?;
        Ok(Self {
            owner,
            item_ids,
            body: encode(&value)?,
        })
    }

    pub fn from_body(owner: QueueRequestOwner, body: Vec<u8>) -> Result<Self, QueueManifestError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Body {
            directory: String,
            item_ids: Vec<String>,
            format: String,
        }
        let raw: Body = decode_input(&body)?;
        if raw.directory != owner.scope.directory || raw.format != "manifest-v1" {
            return Err(QueueManifestError::InvalidInput);
        }
        let item_ids = raw.item_ids.into_iter().map(QueuedItemId::new).collect();
        let mut capture = Self::new(owner, item_ids)?;
        capture.body = body;
        Ok(capture)
    }

    pub fn owner(&self) -> &QueueRequestOwner {
        &self.owner
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    pub fn item_ids(&self) -> &[QueuedItemId] {
        &self.item_ids
    }

    pub async fn submit(&self) -> Result<QueueTakeRecord, QueueManifestError> {
        self.check(post_record(&self.owner, "take", &self.body).await?)
    }

    pub async fn lookup(&self) -> Result<Option<QueueTakeRecord>, QueueManifestError> {
        match self.owner.lookup(None).await? {
            QueueLookup::Unknown => Ok(None),
            QueueLookup::Found(record) => self.check(*record).map(Some),
        }
    }

    fn check(&self, record: QueueRecord) -> Result<QueueTakeRecord, QueueManifestError> {
        match record {
            QueueRecord::Take(record) if record.receipt.item_ids == self.item_ids => Ok(record),
            _ => Err(QueueManifestError::ReceiptMismatch),
        }
    }
}

impl fmt::Debug for QueueTake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueTake")
            .field("owner", &self.owner)
            .field("items", &self.item_ids.len())
            .finish()
    }
}

/// Journal this acknowledgement only after every returned part and verified file
/// is durably owned locally. A lost response can be explicitly repeated.
pub struct QueueAcknowledge {
    owner: QueueRequestOwner,
    receipt: QueueTakeReceipt,
    body: Vec<u8>,
}

impl QueueAcknowledge {
    pub fn new(
        owner: QueueRequestOwner,
        receipt: QueueTakeReceipt,
    ) -> Result<Self, QueueManifestError> {
        validate_receipt_owner(&receipt.identity, &owner)?;
        validate_item_ids(&receipt.item_ids)?;
        let body = encode(&json!({"directory": owner.scope.directory}))?;
        Ok(Self {
            owner,
            receipt,
            body,
        })
    }

    pub fn owner(&self) -> &QueueRequestOwner {
        &self.owner
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub async fn submit(&self) -> Result<QueueTakeRecord, QueueManifestError> {
        match post_record(&self.owner, "acknowledge", &self.body).await? {
            QueueRecord::Take(record)
                if record.receipt == self.receipt
                    && matches!(record.state, QueueTakeState::Acknowledged) =>
            {
                Ok(record)
            }
            _ => Err(QueueManifestError::ReceiptMismatch),
        }
    }
}

impl fmt::Debug for QueueAcknowledge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueAcknowledge")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

/// Explicit authorization of a journaled attempt under the original enqueue ID.
pub struct QueueRetry {
    owner: QueueRequestOwner,
    receipt: QueueEnqueueReceipt,
    attempt: u64,
    body: Vec<u8>,
}

impl QueueRetry {
    pub fn new(
        owner: QueueRequestOwner,
        receipt: QueueEnqueueReceipt,
        attempt: u64,
    ) -> Result<Self, QueueManifestError> {
        validate_receipt_owner(&receipt.identity, &owner)?;
        validate_enqueue_receipt(&receipt)?;
        if attempt == 0 || attempt > MAX_SAFE_INTEGER {
            return Err(QueueManifestError::InvalidInput);
        }
        let body = encode(&json!({"directory": owner.scope.directory, "attempt": attempt}))?;
        Ok(Self {
            owner,
            receipt,
            attempt,
            body,
        })
    }

    pub fn owner(&self) -> &QueueRequestOwner {
        &self.owner
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    pub fn attempt(&self) -> u64 {
        self.attempt
    }

    pub async fn submit(&self) -> Result<QueueEnqueueRecord, QueueManifestError> {
        match post_record(&self.owner, "retry", &self.body).await? {
            QueueRecord::Enqueue(record)
                if record.receipt == self.receipt && record.attempt == self.attempt =>
            {
                Ok(record)
            }
            _ => Err(QueueManifestError::ReceiptMismatch),
        }
    }
}

impl fmt::Debug for QueueRetry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueRetry")
            .field("owner", &self.owner)
            .field("attempt", &self.attempt)
            .finish()
    }
}

/// Immutable server delivery identities. Request hashes are parsed as digests;
/// transport cannot recompute them without a SHA-256 implementation.
#[derive(Clone, PartialEq, Eq)]
pub struct QueueReceiptIdentity {
    pub request_id: String,
    pub session_id: SessionId,
    pub directory: String,
    pub request_hash: [u8; 32],
    pub accepted_at: u64,
}

impl fmt::Debug for QueueReceiptIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueReceiptIdentity")
            .field("request_id", &self.request_id)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueEnqueueReceipt {
    pub identity: QueueReceiptIdentity,
    pub item_id: QueuedItemId,
    pub message_id: MessageId,
    pub context_ids: Vec<MessageId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueTakeReceipt {
    pub identity: QueueReceiptIdentity,
    pub item_ids: Vec<QueuedItemId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueReceipt {
    Enqueue(QueueEnqueueReceipt),
    Take(QueueTakeReceipt),
}

impl QueueReceipt {
    pub fn identity(&self) -> &QueueReceiptIdentity {
        match self {
            Self::Enqueue(receipt) => &receipt.identity,
            Self::Take(receipt) => &receipt.identity,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueEnqueueState {
    Queued,
    Uncertain,
    Dispatched,
    Removed,
    Taken,
    Deleted,
}

/// Server-declared delivery holds. Both retain the accepted durable enqueue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueBlocked {
    /// The command route cannot preserve the captured delivery identity.
    CommandIdentityUnsupported,
    /// The queue server's expanded outgoing prompt exceeds its size policy.
    /// The unattempted item remains queued and can be taken for editing.
    PromptBodyTooLarge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueEnqueueRecord {
    pub receipt: QueueEnqueueReceipt,
    pub state: QueueEnqueueState,
    pub revision: u64,
    pub attempt: u64,
    pub blocked: Option<QueueBlocked>,
}

/// Payload presence is coupled to its authority. Acknowledged has no items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueTakeState {
    Transferring { items: Vec<QueueTakenItem> },
    Acknowledged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueTakeRecord {
    pub receipt: QueueTakeReceipt,
    pub state: QueueTakeState,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueRecord {
    Enqueue(QueueEnqueueRecord),
    Take(QueueTakeRecord),
}

impl QueueRecord {
    pub fn receipt(&self) -> QueueReceipt {
        match self {
            Self::Enqueue(record) => QueueReceipt::Enqueue(record.receipt.clone()),
            Self::Take(record) => QueueReceipt::Take(record.receipt.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueLookup {
    Unknown,
    Found(Box<QueueRecord>),
}

/// A complete captured prefix request. Different queued configurations and
/// metadata stay on their own items; this module never chooses a merge policy.
#[derive(Clone, PartialEq, Eq)]
pub struct QueuePrefixInput {
    pub id: MessageId,
    pub item_ids: Vec<QueuedItemId>,
    pub composer: String,
    pub send_config: QueuedSendConfig,
    pub steer: bool,
    pub prompt_parts: Option<QueuePromptParts>,
}

impl fmt::Debug for QueuePrefixInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuePrefixInput")
            .field("id", &self.id)
            .field("items", &self.item_ids.len())
            .finish_non_exhaustive()
    }
}

pub struct QueuePrefixCapture {
    scope: QueueScope,
    input: QueuePrefixInput,
    body: Vec<u8>,
}

impl QueuePrefixCapture {
    /// Captures an exact directory in the request body. The server must check
    /// that owner atomically before preview, reservation and reservation replay.
    pub fn new(scope: QueueScope, input: QueuePrefixInput) -> Result<Self, QueueManifestError> {
        if !message_id(input.id.as_str()) || js_len(&input.composer) > 200_000 {
            return Err(QueueManifestError::InvalidInput);
        }
        validate_item_ids(&input.item_ids)?;
        validate_config(&input.send_config)?;
        if let Some(parts) = &input.prompt_parts {
            validate_parts(parts)?;
            if parts
                .context
                .iter()
                .any(|context| context.id.as_ref() == Some(&input.id))
            {
                return Err(QueueManifestError::InvalidInput);
            }
        }
        let mut value = json!({"id": input.id.as_str(), "directory": scope.directory,
            "itemIds": input.item_ids.iter().map(QueuedItemId::as_str).collect::<Vec<_>>(),
            "composer": input.composer, "sendConfig": config_value(&input.send_config),
            "delivery": if input.steer { Some("steer") } else { None }, "format": "manifest-v1"});
        if let Some(parts) = &input.prompt_parts {
            value["promptParts"] = parts_value(parts);
        }
        Ok(Self {
            scope,
            input,
            body: encode(&value)?,
        })
    }

    pub fn from_body(scope: QueueScope, body: Vec<u8>) -> Result<Self, QueueManifestError> {
        let wire: PrefixWire = decode_input(&body)?;
        if wire.directory != scope.directory {
            return Err(QueueManifestError::InvalidInput);
        }
        let input = prefix_input(wire, MetadataOrigin::Captured)
            .map_err(|_| QueueManifestError::InvalidInput)?;
        let mut captured = Self::new(scope, input)?;
        captured.body = body;
        Ok(captured)
    }

    pub fn scope(&self) -> &QueueScope {
        &self.scope
    }
    pub fn input(&self) -> &QueuePrefixInput {
        &self.input
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Journal the complete returned candidate and context IDs before reserve.
    pub async fn preview(&self) -> Result<QueueManifestPrefix, QueueManifestError> {
        self.call("preview-prefix").await
    }

    /// Retains the server's FIFO barrier until exact prompt admission. There is
    /// deliberately no fallback to take, even when this endpoint is unsupported.
    pub async fn reserve(&self) -> Result<QueueManifestPrefix, QueueManifestError> {
        self.call("reserve-prefix").await
    }

    async fn call(&self, operation: &str) -> Result<QueueManifestPrefix, QueueManifestError> {
        let request = self
            .scope
            .authorized(Method::POST, self.scope.url(&[operation]))
            .header(CONTENT_TYPE.as_str(), "application/json")
            .body(self.body.clone());
        let bytes = json_body(send(request).await?).await?;
        let wire: PrefixReplyWire = decode(&bytes)?;
        if wire.reservation.directory != self.scope.directory {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        let reservation = prefix_input(wire.reservation, MetadataOrigin::JavaScript)?;
        if reservation != self.input {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        let items = taken_items(wire.items)?;
        if items
            .iter()
            .map(|item| &item.id)
            .ne(self.input.item_ids.iter())
        {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        let context_ids = parse_message_ids(wire.context_ids)?;
        let mut offset = 0;
        for item in &items {
            let count = context_count(&item.item);
            let ids = context_ids
                .get(offset..offset + count)
                .ok_or(QueueManifestError::InvalidResponse)?;
            validate_context_ids(ids, &item.item)?;
            offset += count;
        }
        let composer = self.input.prompt_parts.as_ref();
        let composer_context = composer.map_or(&[][..], |parts| parts.context.as_slice());
        if context_ids.len() != offset + composer_context.len()
            || composer_context
                .iter()
                .zip(&context_ids[offset..])
                .any(|(context, id)| context.id.as_ref().is_some_and(|saved| saved != id))
            || context_ids.contains(&self.input.id)
            || total_entries(&items) + composer.map_or(0, parts_count) > QUEUE_MANIFEST_ENTRIES
        {
            return Err(QueueManifestError::InvalidResponse);
        }
        Ok(QueueManifestPrefix {
            revision: safe_integer(wire.revision)?,
            directory: self.scope.directory.clone(),
            reservation,
            items,
            context_ids,
        })
    }
}

impl fmt::Debug for QueuePrefixCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuePrefixCapture")
            .field("scope", &self.scope)
            .field("input", &self.input)
            .finish()
    }
}

/// No queue snapshot rides this bounded receipt. The returned context IDs are
/// authoritative and must be journaled, never regenerated by the client.
#[derive(Clone, PartialEq, Eq)]
pub struct QueueManifestPrefix {
    pub revision: u64,
    /// Exact directory echoed by the backend and checked against the capture.
    pub directory: String,
    pub reservation: QueuePrefixInput,
    pub items: Vec<QueueTakenItem>,
    pub context_ids: Vec<MessageId>,
}

impl fmt::Debug for QueueManifestPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueManifestPrefix")
            .field("revision", &self.revision)
            .field("reservation", &self.reservation)
            .field("items", &self.items.len())
            .field("context_ids", &self.context_ids)
            .finish_non_exhaustive()
    }
}

/// One non-empty chunk of a structured capture. Ordinals are checked in array
/// order before any allocation, download or ownership acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePayloadChunk {
    pub ordinal: u64,
    pub attachment: QueueAttachmentDescriptor,
}

/// Strict enqueue/composer pointer. Returned indexes cannot enter this type
/// through parsing. Original upload identities remain unchanged on retries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePayloadPointer {
    total_bytes: u64,
    sha256: [u8; 32],
    chunks: Vec<QueuePayloadChunk>,
}

impl QueuePayloadPointer {
    pub fn new(
        total_bytes: u64,
        sha256: [u8; 32],
        chunks: Vec<QueuePayloadChunk>,
    ) -> Result<Self, QueueManifestError> {
        if total_bytes == 0 || total_bytes > QUEUE_PAYLOAD_BYTES as u64 {
            return Err(QueueManifestError::PayloadTooLarge);
        }
        if chunks.is_empty() || chunks.len() > QUEUE_MANIFEST_ENTRIES {
            return Err(QueueManifestError::InvalidInput);
        }
        let mut ids = HashSet::new();
        let mut size = 0;
        for (ordinal, chunk) in chunks.iter().enumerate() {
            validate_descriptor(&chunk.attachment)?;
            if chunk.ordinal != ordinal as u64
                || chunk.attachment.size == 0
                || !ids.insert(chunk.attachment.blob_id)
            {
                return Err(QueueManifestError::InvalidInput);
            }
            size += chunk.attachment.size;
            if size > QUEUE_PAYLOAD_BYTES as u64 {
                return Err(QueueManifestError::PayloadTooLarge);
            }
        }
        if size != total_bytes {
            return Err(QueueManifestError::InvalidInput);
        }
        let pointer = Self {
            total_bytes,
            sha256,
            chunks,
        };
        encode(&pointer.value())?;
        Ok(pointer)
    }

    pub fn from_body(bytes: &[u8]) -> Result<Self, QueueManifestError> {
        let wire: PayloadPointerWire = decode_input(bytes)?;
        payload_pointer(wire).map_err(|_| QueueManifestError::InvalidInput)
    }

    pub fn body(&self) -> Result<Vec<u8>, QueueManifestError> {
        encode(&self.value())
    }
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
    pub fn chunks(&self) -> &[QueuePayloadChunk] {
        &self.chunks
    }

    fn value(&self) -> Value {
        json!({"format":"payload-v1", "payloadManifest":payload_manifest_wire(self)})
    }

    fn verify_bytes(&self, bytes: &[u8], hash: QueueSha256) -> Result<(), QueueManifestError> {
        if bytes.len() as u64 != self.total_bytes {
            return Err(QueueManifestError::PayloadMismatch);
        }
        let mut offset = 0;
        for chunk in &self.chunks {
            let end = offset + chunk.attachment.size as usize;
            if hash(&bytes[offset..end]) != chunk.attachment.sha256 {
                return Err(QueueManifestError::PayloadMismatch);
            }
            offset = end;
        }
        if hash(bytes) != self.sha256 {
            return Err(QueueManifestError::PayloadMismatch);
        }
        Ok(())
    }
}

/// Verified compact root metadata. Encoded size counters attest the queue
/// server's request. Native staged-file sends must measure their actual body.
#[derive(Clone, PartialEq, Eq)]
pub struct QueuePayloadIndex {
    pub directory: String,
    pub send_config: QueuedSendConfig,
    /// Display-only UTF-8. Indexed persistence retains original UTF-16 units.
    pub preview: String,
    /// Display-only UTF-8. Sealed persistence retains the original UTF-16 units.
    pub context_preview: Option<String>,
    pub entry_count: usize,
    pub context_ids: Vec<MessageId>,
    pub attachment_blob_ids: Vec<[u8; 32]>,
    pub prompt_bytes: u64,
    pub admission_bytes: u64,
    pub structured_bytes: u64,
    pub legacy_attachment_ids: Option<Vec<Option<String>>>,
}

impl fmt::Debug for QueuePayloadIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuePayloadIndex")
            .field("entry_count", &self.entry_count)
            .field("context_ids", &self.context_ids)
            .field("prompt_bytes", &self.prompt_bytes)
            .field("admission_bytes", &self.admission_bytes)
            .finish_non_exhaustive()
    }
}

/// A server-returned pointer with a checked schema-normalized index SHA-256.
/// It is separate from enqueue input so indexes are never echoed accidentally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueIndexedPayload {
    pointer: QueuePayloadPointer,
    index: QueuePayloadIndex,
    index_hash: [u8; 32],
    // Display text may replace a split surrogate; sealed bytes never do.
    preview: PayloadPreview,
    context_preview: Option<PayloadPreview>,
}

impl QueueIndexedPayload {
    pub fn from_body(
        bytes: &[u8],
        directory: &str,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        indexed_payload(decode(bytes)?, directory, hash)
    }

    pub fn pointer(&self) -> &QueuePayloadPointer {
        &self.pointer
    }
    pub fn index(&self) -> &QueuePayloadIndex {
        &self.index
    }
    pub fn index_hash(&self) -> [u8; 32] {
        self.index_hash
    }

    /// Compact persistence bytes. These are recovery metadata, never enqueue
    /// input. Store the original request body separately for exact repetition.
    pub fn body(&self) -> Result<Vec<u8>, QueueManifestError> {
        let mut bytes = self.pointer.body()?;
        bytes.pop();
        bytes.extend_from_slice(b",\"payloadIndex\":");
        bytes.extend_from_slice(&payload_index_bytes(
            &self.index,
            &self.preview,
            self.context_preview.as_ref(),
        )?);
        bytes.extend_from_slice(b",\"payloadIndexHash\":");
        append_json(&mut bytes, &hex_digest(&self.index_hash))?;
        bytes.push(b'}');
        if bytes.len() > QUEUE_MANIFEST_BYTES {
            return Err(QueueManifestError::MetadataTooLarge);
        }
        Ok(bytes)
    }

    /// Verify every chunk and whole-capture digest before parsing. This method
    /// restores frozen context and legacy attachment IDs and checks owner,
    /// configuration, entry count and hidden attachment roots against the index.
    /// Bytes and expanded parts belong in the local spool/active operation, not
    /// in duplicate expanded journal records. Attachment downloads still require
    /// their own digest checks before local acknowledgement.
    pub fn materialize(
        &self,
        bytes: Vec<u8>,
        usage: QueuePayloadUse,
        hash: QueueSha256,
    ) -> Result<QueuePayloadDocument, QueueManifestError> {
        self.pointer.verify_bytes(&bytes, hash)?;
        let mut document =
            QueuePayloadDocument::from_bytes(bytes, &self.index.directory, usage, hash)?;
        document
            .item
            .bind_index(&self.index, &self.preview, self.context_preview.as_ref())?;
        document.materialized_index = Some(self.index_hash);
        Ok(document)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuePayloadUse {
    Enqueue,
    Composer,
}

/// Typed legacy file metadata retains its original fields. `server_path` is
/// display metadata only; bytes always come from the validated inline data URL.
#[derive(Clone, PartialEq, Eq)]
pub struct QueuePayloadLegacyAttachment {
    pub id: Option<String>,
    pub filename: String,
    pub mime_type: String,
    /// Original legacy metadata; actual decoded length is checked separately.
    pub size: serde_json::Number,
    pub source: String,
    pub server_path: Option<String>,
    /// Original attachment identity shown to the model. This never grants a file read.
    pub agent_path: Option<String>,
    data_url: String,
}

impl QueuePayloadLegacyAttachment {
    /// At most 20 MiB of decoded prepared bytes, suitable for the existing
    /// attachment spool. This never reads a captured server path.
    pub fn bytes(&self) -> Result<Vec<u8>, QueueManifestError> {
        inline_payload_bytes(&self.data_url)
    }
    pub fn data_url(&self) -> &str {
        &self.data_url
    }
}

impl fmt::Debug for QueuePayloadLegacyAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuePayloadLegacyAttachment")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct QueuePayloadLegacyItem {
    pub content: String,
    pub text: String,
    pub agent_mention: Option<String>,
    pub context_preview: Option<PayloadPreview>,
    pub context: Vec<QueueLegacyContext>,
    pub send_config: QueuedSendConfig,
    pub attachments: Vec<QueuePayloadLegacyAttachment>,
    /// Explicit references from the legacy capture's optional promptParts.
    pub file_references: Vec<QueueFileReference>,
}

impl fmt::Debug for QueuePayloadLegacyItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuePayloadLegacyItem")
            .field("files", &self.attachments.len())
            .field("contexts", &self.context.len())
            .finish_non_exhaustive()
    }
}

/// Complete structured capture union. No fields are projected into a lossy
/// prompt, and individual item selections and metadata remain independent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueuePayloadItem {
    Manifest(QueueManifestItem),
    Legacy(QueuePayloadLegacyItem),
}

impl QueuePayloadItem {
    pub fn send_config(&self) -> &QueuedSendConfig {
        match self {
            Self::Manifest(item) => &item.send_config,
            Self::Legacy(item) => &item.send_config,
        }
    }

    fn context_bindings(&self) -> Vec<Option<MessageId>> {
        match self {
            Self::Manifest(item) => {
                let mut ids = vec![None; legacy_count(&item.context)];
                ids.extend(item.prompt_parts.context.iter().map(|part| part.id.clone()));
                ids
            }
            Self::Legacy(item) => vec![None; legacy_count(&item.context)],
        }
    }

    fn bind_index(
        &mut self,
        index: &QueuePayloadIndex,
        sealed_preview: &PayloadPreview,
        sealed_context_preview: Option<&PayloadPreview>,
    ) -> Result<(), QueueManifestError> {
        let bindings = self.context_bindings();
        if self.send_config() != &index.send_config
            || bindings.len() != index.context_ids.len()
            || bindings
                .iter()
                .zip(&index.context_ids)
                .any(|(saved, id)| saved.as_ref().is_some_and(|saved| saved != id))
        {
            return Err(QueueManifestError::PayloadMismatch);
        }
        let (count, preview, context_preview, roots) = match self {
            Self::Manifest(item) => {
                if index.legacy_attachment_ids.is_some() {
                    return Err(QueueManifestError::PayloadMismatch);
                }
                let mut roots = Vec::new();
                for file in &item.attachment_manifest {
                    if !roots.contains(&file.attachment.blob_id) {
                        roots.push(file.attachment.blob_id);
                    }
                }
                for (context, id) in item
                    .prompt_parts
                    .context
                    .iter_mut()
                    .zip(index.context_ids.iter().skip(legacy_count(&item.context)))
                {
                    context.id = Some(id.clone());
                }
                (
                    item.attachment_manifest.len()
                        + parts_count(&item.prompt_parts)
                        + legacy_count(&item.context),
                    payload_preview_units(&item.content, &item.text),
                    item.context_preview.as_ref(),
                    roots,
                )
            }
            Self::Legacy(item) => {
                let ids = index
                    .legacy_attachment_ids
                    .as_ref()
                    .ok_or(QueueManifestError::PayloadMismatch)?;
                if ids.len() != item.attachments.len() {
                    return Err(QueueManifestError::PayloadMismatch);
                }
                for (file, binding) in item.attachments.iter_mut().zip(ids) {
                    if let Some(binding) = binding {
                        if file.id.as_ref().is_some_and(|id| id != binding) {
                            return Err(QueueManifestError::PayloadMismatch);
                        }
                        file.id = Some(binding.clone());
                    } else if file.id.as_deref().is_none_or(|id| js_len(id) <= 256) {
                        return Err(QueueManifestError::PayloadMismatch);
                    }
                }
                (
                    item.attachments.len()
                        + item.file_references.len()
                        + usize::from(item.agent_mention.is_some())
                        + legacy_count(&item.context),
                    payload_preview_units(&item.content, &item.text),
                    item.context_preview.as_ref(),
                    Vec::new(),
                )
            }
        };
        if count != index.entry_count
            || &preview != sealed_preview
            || context_preview != sealed_context_preview
            || roots != index.attachment_blob_ids
        {
            return Err(QueueManifestError::PayloadMismatch);
        }
        Ok(())
    }
}

/// Parse the one UTF-16 display field directly; arbitrary metadata continues to
/// use Value and therefore cannot smuggle invalid Unicode into authoritative text.
struct PayloadCaptureItem {
    fields: Map<String, Value>,
    context_preview: Option<PayloadPreview>,
}

impl<'de> Deserialize<'de> for PayloadCaptureItem {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ItemVisitor;
        impl<'de> serde::de::Visitor<'de> for ItemVisitor {
            type Value = PayloadCaptureItem;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a structured queue item")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut fields = Map::new();
                let mut context_preview = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "contextPreview" {
                        if context_preview.is_some() {
                            return Err(serde::de::Error::duplicate_field("contextPreview"));
                        }
                        context_preview = Some(map.next_value::<PayloadPreview>()?);
                    } else if fields.insert(key, map.next_value::<Value>()?).is_some() {
                        return Err(serde::de::Error::custom("duplicate queue item field"));
                    }
                }
                Ok(PayloadCaptureItem {
                    fields,
                    context_preview,
                })
            }
        }
        deserializer.deserialize_map(ItemVisitor)
    }
}

/// Active capture bytes and parsed parts. Durable storage uses a compact spool
/// descriptor containing length and digest, never this expanded structure.
pub struct QueuePayloadDocument {
    bytes: Vec<u8>,
    directory: String,
    item: QueuePayloadItem,
    sha256: [u8; 32],
    usage: QueuePayloadUse,
    materialized_index: Option<[u8; 32]>,
}

/// Compact evidence derived only from a fully verified, sealed payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePayloadFilesProof {
    capture_sha256: [u8; 32],
    index_sha256: [u8; 32],
    files: Vec<QueuePayloadFileProof>,
}

impl QueuePayloadFilesProof {
    /// The complete original capture's digest.
    pub fn capture_sha256(&self) -> &[u8; 32] {
        &self.capture_sha256
    }
    /// The index seal checked during materialization.
    pub fn index_sha256(&self) -> &[u8; 32] {
        &self.index_sha256
    }
    /// Original prepared files. Duplicate manifest blob IDs share one proof.
    pub fn files(&self) -> &[QueuePayloadFileProof] {
        &self.files
    }
    /// Heap bytes retained by this compact proof for bounded worker admission.
    pub fn retained_bytes(&self) -> usize {
        self.files.capacity() * std::mem::size_of::<QueuePayloadFileProof>()
    }
}

/// A byte descriptor bound to its original captured attachment identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePayloadFileProof {
    source: QueuePayloadFileSource,
    byte_len: u64,
    sha256: [u8; 32],
}

/// Manifest blob identity or array position in a legacy capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueuePayloadFileSource {
    Manifest([u8; 32]),
    Legacy(usize),
}

impl QueuePayloadFileProof {
    /// The immutable source binding in the original capture.
    pub fn source(&self) -> &QueuePayloadFileSource {
        &self.source
    }
    /// The exact prepared-byte length.
    pub fn byte_len(&self) -> u64 {
        self.byte_len
    }
    /// The exact prepared-byte SHA-256.
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}

impl QueuePayloadDocument {
    pub fn from_bytes(
        bytes: Vec<u8>,
        directory: &str,
        usage: QueuePayloadUse,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        if bytes.is_empty() || bytes.len() > QUEUE_PAYLOAD_BYTES {
            return Err(QueueManifestError::PayloadTooLarge);
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Capture {
            directory: String,
            item: PayloadCaptureItem,
        }
        // serde rejects malformed UTF-8, a BOM and trailing JSON. The typed inner
        // parsers reject nulls and unknown fields at every protocol level.
        validate_json_text(&bytes).map_err(|_| QueueManifestError::InvalidInput)?;
        let raw: Capture =
            serde_json::from_slice(&bytes).map_err(|_| QueueManifestError::InvalidInput)?;
        if raw.directory != directory || !valid_payload_directory(directory) {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        let mut item = parse_payload_item(Value::Object(raw.item.fields), usage)?;
        match &mut item {
            QueuePayloadItem::Manifest(item) => item.context_preview = raw.item.context_preview,
            QueuePayloadItem::Legacy(item) => item.context_preview = raw.item.context_preview,
        }
        Ok(Self {
            sha256: hash(&bytes),
            bytes,
            directory: directory.to_owned(),
            item,
            usage,
            materialized_index: None,
        })
    }

    pub fn from_manifest(
        directory: &str,
        item: QueueManifestItem,
        usage: QueuePayloadUse,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        validate_item_for(&item, usage == QueuePayloadUse::Composer)?;
        let bytes = encode_payload_capture(
            directory,
            item_value(&item),
            item.context_preview.as_ref(),
            PayloadEncoding::Manifest,
        )?;
        Self::from_bytes(bytes, directory, usage, hash)
    }

    /// Re-encode an edited/materialized item with all restored attachment and
    /// explicit context identities. The old pointer is never edited or reused.
    pub fn from_item(
        directory: &str,
        item: &QueuePayloadItem,
        usage: QueuePayloadUse,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        let value = match item {
            QueuePayloadItem::Manifest(item) => {
                validate_item_for(item, usage == QueuePayloadUse::Composer)?;
                item_value(item)
            }
            QueuePayloadItem::Legacy(item) => {
                // Check the caller's integers before the JavaScript-number formatter
                // can round them. Values decoded from JavaScript remain finite f64.
                for context in &item.context {
                    if let QueueLegacyContext::Context { metadata, .. } = context {
                        validate_metadata(metadata)?;
                    }
                }
                if item
                    .attachments
                    .iter()
                    .any(|file| captured_json_number(&file.size).is_none())
                {
                    return Err(QueueManifestError::InvalidInput);
                }
                let context: Vec<_> = item.context.iter().map(legacy_context_value).collect();
                let attachments: Vec<_> = item.attachments.iter().map(|file| {
                    let mut value = json!({"filename":file.filename,"mimeType":file.mime_type,"dataUrl":file.data_url});
                    if file.size.as_f64() != Some(0.0) { value["size"] = json!(file.size); }
                    if file.source != "local" { value["source"] = json!(file.source); }
                    if let Some(id) = &file.id { value["id"] = json!(id); }
                    if let Some(path) = &file.server_path { value["serverPath"] = json!(path); }
                    if let Some(path) = &file.agent_path { value["agentPath"] = json!(path); }
                    value
                }).collect();
                let mut value = json!({"sendConfig":config_value(&item.send_config)});
                if !item.content.is_empty() {
                    value["content"] = json!(item.content);
                }
                if item.text != item.content {
                    value["text"] = json!(item.text);
                }
                if !context.is_empty() {
                    value["context"] = json!(context);
                }
                if !attachments.is_empty() {
                    value["attachments"] = json!(attachments);
                }
                if let Some(mention) = &item.agent_mention {
                    value["agentMention"] = json!(mention);
                }
                if !item.file_references.is_empty() {
                    value["promptParts"] = json!({"fileReferences":item.file_references});
                }
                value
            }
        };
        let (preview, encoding) = match item {
            QueuePayloadItem::Manifest(item) => {
                (item.context_preview.as_ref(), PayloadEncoding::Manifest)
            }
            QueuePayloadItem::Legacy(item) => {
                (item.context_preview.as_ref(), PayloadEncoding::Legacy)
            }
        };
        let bytes = encode_payload_capture(directory, value, preview, encoding)?;
        Self::from_bytes(bytes, directory, usage, hash)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
    pub fn directory(&self) -> &str {
        &self.directory
    }
    pub fn item(&self) -> &QueuePayloadItem {
        &self.item
    }

    /// Requires sealed materialization. Freshly parsed or edited captures grant
    /// no transfer proof. Legacy files are decoded and hashed one at a time.
    pub fn files_proof(
        &self,
        hash: QueueSha256,
    ) -> Result<QueuePayloadFilesProof, QueueManifestError> {
        let index_sha256 = self
            .materialized_index
            .ok_or(QueueManifestError::PayloadMismatch)?;
        let mut files: Vec<QueuePayloadFileProof> = Vec::new();
        match &self.item {
            QueuePayloadItem::Manifest(item) => {
                for file in &item.attachment_manifest {
                    let original = &file.attachment;
                    let proof = QueuePayloadFileProof {
                        source: QueuePayloadFileSource::Manifest(original.blob_id),
                        byte_len: original.size,
                        sha256: original.sha256,
                    };
                    if let Some(previous) = files
                        .iter()
                        .find(|previous| previous.source == proof.source)
                    {
                        if previous != &proof {
                            return Err(QueueManifestError::AttachmentMismatch);
                        }
                    } else {
                        files.push(proof);
                    }
                }
            }
            QueuePayloadItem::Legacy(item) => {
                for (ordinal, file) in item.attachments.iter().enumerate() {
                    let bytes = file.bytes()?;
                    files.push(QueuePayloadFileProof {
                        source: QueuePayloadFileSource::Legacy(ordinal),
                        byte_len: bytes.len() as u64,
                        sha256: hash(&bytes),
                    });
                }
            }
        }
        Ok(QueuePayloadFilesProof {
            capture_sha256: self.sha256,
            index_sha256,
            files,
        })
    }
    pub fn chunk_count(&self) -> usize {
        self.bytes.len().div_ceil(QUEUE_BLOB_BYTES)
    }

    /// One explicit upload capture at a time. Retain this request owner and the
    /// exact bytes before PUT. Neither uncertainty nor retry changes chunk IDs.
    pub fn chunk_upload(
        &self,
        owner: QueueRequestOwner,
        ordinal: usize,
        hash: QueueSha256,
    ) -> Result<QueueUpload, QueueManifestError> {
        if owner.scope.directory != self.directory {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        let bytes = self
            .bytes
            .chunks(QUEUE_BLOB_BYTES)
            .nth(ordinal)
            .ok_or(QueueManifestError::InvalidInput)?;
        QueueUpload::new(
            owner,
            &format!("payload-{ordinal}"),
            &format!("payload-{ordinal}.json"),
            "application/json",
            hash(bytes),
            bytes.to_vec(),
        )
    }

    /// Assemble the compact pointer after the caller retains all acknowledged
    /// upload descriptors. Every descriptor must match the exact planned chunk.
    pub fn pointer(
        &self,
        descriptors: Vec<QueueAttachmentDescriptor>,
        hash: QueueSha256,
    ) -> Result<QueuePayloadPointer, QueueManifestError> {
        if descriptors.len() != self.chunk_count() {
            return Err(QueueManifestError::InvalidInput);
        }
        let chunks = descriptors
            .into_iter()
            .enumerate()
            .map(|(ordinal, attachment)| QueuePayloadChunk {
                ordinal: ordinal as u64,
                attachment,
            })
            .collect();
        let pointer = QueuePayloadPointer::new(self.bytes.len() as u64, self.sha256, chunks)?;
        pointer.verify_bytes(&self.bytes, hash)?;
        Ok(pointer)
    }
}

impl fmt::Debug for QueuePayloadDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuePayloadDocument")
            .field("bytes", &self.bytes.len())
            .field("usage", &self.usage)
            .finish_non_exhaustive()
    }
}

/// Native prepared bytes, identified by their array ordinal. These fields name
/// no queue upload, server blob or transfer receipt. `name` is model-facing
/// metadata only; staged byte access is supplied separately by the file owner.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueLocalFile {
    pub name: String,
    pub mime_type: String,
    pub byte_len: u64,
    pub sha256: [u8; 32],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<QueueMention>,
}

impl fmt::Debug for QueueLocalFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueLocalFile")
            .field("byte_len", &self.byte_len)
            .finish_non_exhaustive()
    }
}

/// Complete local edit/send content. This type preserves rich fields which the
/// native draft and PromptParts cannot represent. It grants no queue ownership.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueLocalItem {
    pub content: String,
    pub text: String,
    pub files: Vec<QueueLocalFile>,
    #[serde(serialize_with = "serialize_local_configuration")]
    pub send_config: QueuedSendConfig,
    pub prompt_parts: QueuePromptParts,
}

fn serialize_local_configuration<S: serde::Serializer>(
    config: &QueuedSendConfig,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    config_value(config).serialize(serializer)
}

impl fmt::Debug for QueueLocalItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueLocalItem")
            .field("files", &self.files.len())
            .field("parts", &self.prompt_parts)
            .finish_non_exhaustive()
    }
}

/// Immutable local capture with caller-retained context IDs. Its private source
/// bytes use `native-local-v1`, not an enqueue pointer or a queue index. There is
/// deliberately no QueuePayloadFilesProof, acknowledgement or upload capability.
pub struct QueueLocalCapture {
    bytes: Vec<u8>,
    directory: String,
    item: QueueLocalItem,
    context_ids: Vec<MessageId>,
    sha256: [u8; 32],
}

impl fmt::Debug for QueueLocalCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueLocalCapture")
            .field("bytes", &self.bytes.len())
            .field("contexts", &self.context_ids.len())
            .finish_non_exhaustive()
    }
}

impl QueueLocalCapture {
    /// Capture fresh native fields without queue descriptors. Validate Rust
    /// numbers before serialization, and bind supplied IDs without minting any.
    /// Empty content is allowed as one input to a larger send, as for composers.
    pub fn new(
        directory: &str,
        mut item: QueueLocalItem,
        context_ids: Vec<MessageId>,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        validate_local_capture(directory, &mut item, &context_ids)?;
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Capture<'a> {
            format: &'static str,
            directory: &'a str,
            item: &'a QueueLocalItem,
            context_ids: Vec<&'a str>,
        }
        let capture = Capture {
            format: "native-local-v1",
            directory,
            item: &item,
            context_ids: context_ids.iter().map(MessageId::as_str).collect(),
        };
        let mut count = PayloadByteCount(0);
        write_queue_request_json(&mut count, &capture).map_err(|error| {
            if error.is_io() {
                QueueManifestError::PayloadTooLarge
            } else {
                QueueManifestError::InvalidInput
            }
        })?;
        let mut bytes = Vec::with_capacity(count.0);
        write_queue_request_json(&mut bytes, &capture)
            .map_err(|_| QueueManifestError::InvalidInput)?;
        Ok(Self {
            sha256: hash(&bytes),
            bytes,
            directory: directory.to_owned(),
            item,
            context_ids,
        })
    }

    /// Restore exact local source bytes from the caller's verified private spool.
    /// JSON numbers retain the same JavaScript normalization as payload captures.
    pub fn from_bytes(
        bytes: Vec<u8>,
        directory: &str,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        if bytes.is_empty() || bytes.len() > QUEUE_PAYLOAD_BYTES {
            return Err(QueueManifestError::PayloadTooLarge);
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Capture {
            format: String,
            directory: String,
            item: LocalItemWire,
            context_ids: Vec<String>,
        }
        validate_json_text(&bytes).map_err(|_| QueueManifestError::InvalidInput)?;
        let capture: Capture =
            serde_json::from_slice(&bytes).map_err(|_| QueueManifestError::InvalidInput)?;
        if capture.format != "native-local-v1" || capture.directory != directory {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        let mut item = QueueLocalItem {
            content: capture.item.content,
            text: capture.item.text,
            files: capture
                .item
                .files
                .into_iter()
                .map(|file| {
                    Ok(QueueLocalFile {
                        name: file.name,
                        mime_type: file.mime_type,
                        byte_len: file.byte_len,
                        sha256: file.sha256,
                        description: file.description,
                        mention: file.mention.map(mention).transpose()?,
                    })
                })
                .collect::<Result<_, QueueManifestError>>()?,
            send_config: config(capture.item.send_config)?,
            prompt_parts: parts(capture.item.prompt_parts, MetadataOrigin::JavaScript)?,
        };
        let context_ids =
            parse_message_ids(capture.context_ids).map_err(|_| QueueManifestError::InvalidInput)?;
        validate_local_capture(directory, &mut item, &context_ids)?;
        Ok(Self {
            sha256: hash(&bytes),
            bytes,
            directory: directory.to_owned(),
            item,
            context_ids,
        })
    }

    /// Explicitly recapture a genuine edited/materialized queue document as a
    /// local edit. Upload provenance and transfer authority do not survive this
    /// conversion. Legacy instructions keep their supplied leading IDs and order.
    pub fn from_document(
        document: QueuePayloadDocument,
        context_ids: Vec<MessageId>,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        let mut contexts = Vec::new();
        let mut push = |text: String, metadata: Option<Metadata>| {
            contexts.push(QueuePromptContext {
                id: None,
                text,
                description: None,
                metadata,
            })
        };
        let (content, text, files, send_config, mut prompt_parts, legacy) = match document.item {
            QueuePayloadItem::Manifest(item) => (
                item.content,
                item.text,
                item.attachment_manifest
                    .into_iter()
                    .map(|file| QueueLocalFile {
                        name: file.agent_path.unwrap_or(file.attachment.filename),
                        mime_type: file.attachment.mime_type,
                        byte_len: file.attachment.size,
                        sha256: file.attachment.sha256,
                        description: file.description,
                        mention: file.mention,
                    })
                    .collect(),
                item.send_config,
                item.prompt_parts,
                item.context,
            ),
            QueuePayloadItem::Legacy(item) => {
                let mut files = Vec::with_capacity(item.attachments.len());
                for file in item.attachments {
                    let bytes = file.bytes()?;
                    files.push(QueueLocalFile {
                        name: file.agent_path.unwrap_or(file.filename),
                        mime_type: file.mime_type,
                        byte_len: bytes.len() as u64,
                        sha256: hash(&bytes),
                        description: None,
                        mention: None,
                    });
                }
                let mut prompt_parts = QueuePromptParts {
                    file_references: item.file_references,
                    ..QueuePromptParts::default()
                };
                if let Some(name) = item.agent_mention {
                    prompt_parts.agents.push(QueueAgent {
                        name,
                        mention: None,
                    });
                }
                (
                    item.content,
                    item.text,
                    files,
                    item.send_config,
                    prompt_parts,
                    item.context,
                )
            }
        };
        for entry in legacy {
            match entry {
                QueueLegacyContext::Context {
                    text,
                    metadata,
                    instructions,
                } => {
                    if let Some(instructions) = instructions {
                        push(instructions, None);
                    }
                    push(text, Some(metadata));
                }
                QueueLegacyContext::Instruction { text }
                | QueueLegacyContext::Synthetic { text } => push(text, None),
            }
        }
        contexts.append(&mut prompt_parts.context);
        prompt_parts.context = contexts;
        Self::new(
            &document.directory,
            QueueLocalItem {
                content,
                text,
                files,
                send_config,
                prompt_parts,
            },
            context_ids,
            hash,
        )
    }

    /// Consume a working local capture and replace its context IDs in send order.
    /// The caller owns occurrence-ledger admission. This grants no queue authority.
    /// Identical IDs retain the exact source bytes and digest, including earlier
    /// numeric spellings. Changed IDs recapture all fields through `Self::new`.
    pub fn rebind_context_ids(
        mut self,
        ids: Vec<MessageId>,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        if ids.len() != self.context_ids.len() {
            return Err(QueueManifestError::InvalidInput);
        }
        if ids == self.context_ids {
            return Ok(self);
        }
        for (context, id) in self.item.prompt_parts.context.iter_mut().zip(&ids) {
            context.id = Some(id.clone());
        }
        Self::new(&self.directory, self.item, ids, hash)
    }

    /// Original complete source bytes, chunked by the existing private spool.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Complete source SHA-256, with no queue index seal.
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
    /// Exact source directory spelling.
    pub fn directory(&self) -> &str {
        &self.directory
    }
    /// Complete immutable local fields, including display and delivered text.
    pub fn item(&self) -> &QueueLocalItem {
        &self.item
    }
    /// Caller-captured stable IDs in synthetic dispatch order.
    pub fn context_ids(&self) -> &[MessageId] {
        &self.context_ids
    }
}

fn validate_local_capture(
    directory: &str,
    item: &mut QueueLocalItem,
    ids: &[MessageId],
) -> Result<(), QueueManifestError> {
    if !valid_payload_directory(directory)
        || item.files.len() + parts_count(&item.prompt_parts) > QUEUE_MANIFEST_ENTRIES
        || ids.len() != item.prompt_parts.context.len()
        || ids.iter().any(|id| !message_id(id.as_str()))
        || ids.iter().collect::<HashSet<_>>().len() != ids.len()
    {
        return Err(QueueManifestError::InvalidInput);
    }
    validate_config(&item.send_config)?;
    validate_parts(&item.prompt_parts)?;
    for (context, id) in item.prompt_parts.context.iter_mut().zip(ids) {
        if context.id.as_ref().is_some_and(|saved| saved != id) {
            return Err(QueueManifestError::InvalidInput);
        }
        context.id = Some(id.clone());
    }
    for file in &item.files {
        if file.name.is_empty()
            || file.mime_type.is_empty()
            || file.byte_len > QUEUE_BLOB_BYTES as u64
        {
            return Err(QueueManifestError::InvalidInput);
        }
        if let Some(mention) = &file.mention {
            validate_mention(mention)?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalItemWire {
    content: String,
    text: String,
    files: Vec<LocalFileWire>,
    send_config: ConfigWire,
    prompt_parts: PartsWire,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalFileWire {
    name: String,
    mime_type: String,
    byte_len: u64,
    sha256: [u8; 32],
    #[serde(default, deserialize_with = "optional")]
    description: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    mention: Option<MentionWire>,
}

/// Compact payload enqueue, retaining only identity bindings beside its exact
/// request body. The full document stays in the caller's verified local spool.
pub struct QueuePayloadEnqueue {
    owner: QueueRequestOwner,
    pointer: QueuePayloadPointer,
    context_bindings: Vec<Option<MessageId>>,
    body: Vec<u8>,
}

impl QueuePayloadEnqueue {
    pub fn new(
        owner: QueueRequestOwner,
        pointer: QueuePayloadPointer,
        document: &QueuePayloadDocument,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        if owner.scope.directory != document.directory
            || pointer.total_bytes != document.bytes.len() as u64
            || pointer.sha256 != document.sha256
            || document.usage != QueuePayloadUse::Enqueue
        {
            return Err(QueueManifestError::InvalidInput);
        }
        pointer.verify_bytes(document.bytes(), hash)?;
        let value = json!({"directory":owner.scope.directory,"item":pointer.value()});
        capture_bound("enqueue", &owner, &value)?;
        Ok(Self {
            owner,
            pointer,
            context_bindings: document.item.context_bindings(),
            body: encode(&value)?,
        })
    }

    pub fn from_body(
        owner: QueueRequestOwner,
        body: Vec<u8>,
        document: &QueuePayloadDocument,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Body {
            directory: String,
            item: PayloadPointerWire,
        }
        let raw: Body = decode_input(&body)?;
        if raw.directory != owner.scope.directory {
            return Err(QueueManifestError::InvalidInput);
        }
        let pointer = payload_pointer(raw.item).map_err(|_| QueueManifestError::InvalidInput)?;
        let mut capture = Self::new(owner, pointer, document, hash)?;
        capture.body = body;
        Ok(capture)
    }

    pub fn owner(&self) -> &QueueRequestOwner {
        &self.owner
    }
    pub fn pointer(&self) -> &QueuePayloadPointer {
        &self.pointer
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub async fn submit(&self) -> Result<QueueEnqueueRecord, QueueManifestError> {
        self.check(post_record(&self.owner, "enqueue", &self.body).await?)
    }

    pub async fn lookup(&self) -> Result<Option<QueueEnqueueRecord>, QueueManifestError> {
        match self.owner.lookup(None).await? {
            QueueLookup::Unknown => Ok(None),
            QueueLookup::Found(record) => self.check(*record).map(Some),
        }
    }

    fn check(&self, record: QueueRecord) -> Result<QueueEnqueueRecord, QueueManifestError> {
        let QueueRecord::Enqueue(record) = record else {
            return Err(QueueManifestError::ReceiptMismatch);
        };
        if record.receipt.context_ids.len() != self.context_bindings.len()
            || self
                .context_bindings
                .iter()
                .zip(&record.receipt.context_ids)
                .any(|(saved, id)| saved.as_ref().is_some_and(|saved| saved != id))
        {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        Ok(record)
    }
}

impl fmt::Debug for QueuePayloadEnqueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuePayloadEnqueue")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePayloadTakenItem {
    pub id: QueuedItemId,
    pub created_at: u64,
    pub request_id: Option<String>,
    pub payload: QueueIndexedPayload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueuePayloadTakeState {
    Transferring { items: Vec<QueuePayloadTakenItem> },
    Acknowledged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePayloadTakeRecord {
    pub receipt: QueueTakeReceipt,
    pub state: QueuePayloadTakeState,
    pub revision: u64,
}

/// Payload take is an edit/handoff. Only explicit `QueueAcknowledge` after the
/// local durable-spool barrier releases server payload ownership. Queue-first
/// sends use prefix reservation instead, retaining the admission FIFO barrier.
pub struct QueuePayloadTake {
    owner: QueueRequestOwner,
    item_ids: Vec<QueuedItemId>,
    body: Vec<u8>,
    hash: QueueSha256,
}

impl QueuePayloadTake {
    pub fn new(
        owner: QueueRequestOwner,
        item_ids: Vec<QueuedItemId>,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        validate_item_ids(&item_ids)?;
        let value = json!({"directory":owner.scope.directory,"itemIds":item_ids.iter().map(QueuedItemId::as_str).collect::<Vec<_>>(),"format":"payload-v1"});
        capture_bound("take", &owner, &value)?;
        Ok(Self {
            owner,
            item_ids,
            body: encode(&value)?,
            hash,
        })
    }

    pub fn from_body(
        owner: QueueRequestOwner,
        body: Vec<u8>,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Body {
            directory: String,
            item_ids: Vec<String>,
            format: String,
        }
        let raw: Body = decode_input(&body)?;
        if raw.directory != owner.scope.directory || raw.format != "payload-v1" {
            return Err(QueueManifestError::InvalidInput);
        }
        let mut capture = Self::new(
            owner,
            raw.item_ids.into_iter().map(QueuedItemId::new).collect(),
            hash,
        )?;
        capture.body = body;
        Ok(capture)
    }

    pub fn owner(&self) -> &QueueRequestOwner {
        &self.owner
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    pub fn item_ids(&self) -> &[QueuedItemId] {
        &self.item_ids
    }

    pub async fn submit(&self) -> Result<QueuePayloadTakeRecord, QueueManifestError> {
        let request = self
            .owner
            .scope
            .authorized(Method::POST, self.owner.url(&["take"], false))
            .header(CONTENT_TYPE.as_str(), "application/json")
            .body(self.body.clone());
        self.parse_reply(&json_body(send(request).await?).await?)
    }

    pub async fn lookup(&self) -> Result<Option<QueuePayloadTakeRecord>, QueueManifestError> {
        let request = self
            .owner
            .scope
            .authorized(Method::GET, self.owner.url(&[], true));
        let body = match json_body(send(request).await?).await {
            Err(QueueManifestError::UnknownRequest) => return Ok(None),
            result => result?,
        };
        self.parse_reply(&body).map(Some)
    }

    /// Also validates a compact receipt restored from local durable storage.
    pub fn parse_reply(&self, bytes: &[u8]) -> Result<QueuePayloadTakeRecord, QueueManifestError> {
        let wire: PayloadTakeReplyWire = decode(bytes)?;
        let raw = wire.receipt;
        if raw.version != 1 || raw.kind != "take" {
            return Err(QueueManifestError::InvalidResponse);
        }
        let receipt = QueueTakeReceipt {
            identity: receipt_identity(
                raw.request_id,
                raw.session_id,
                raw.directory,
                raw.request_hash,
                raw.accepted_at,
            )?,
            item_ids: raw.item_ids.into_iter().map(QueuedItemId::new).collect(),
        };
        validate_receipt_owner(&receipt.identity, &self.owner)?;
        if receipt.item_ids != self.item_ids {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        let state = match (wire.state.as_str(), wire.items) {
            ("acknowledged", None) => QueuePayloadTakeState::Acknowledged,
            ("transferring", Some(items)) => {
                let items = payload_taken_items(items, &self.owner.scope.directory, self.hash)?;
                if items.iter().map(|item| &item.id).ne(self.item_ids.iter()) {
                    return Err(QueueManifestError::ReceiptMismatch);
                }
                QueuePayloadTakeState::Transferring { items }
            }
            _ => return Err(QueueManifestError::InvalidResponse),
        };
        Ok(QueuePayloadTakeRecord {
            receipt,
            state,
            revision: safe_integer(wire.revision)?,
        })
    }
}

impl fmt::Debug for QueuePayloadTake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuePayloadTake")
            .field("owner", &self.owner)
            .field("items", &self.item_ids.len())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePayloadPrefixInput {
    pub id: MessageId,
    pub item_ids: Vec<QueuedItemId>,
    pub composer: QueuePayloadPointer,
    pub steer: bool,
}

pub struct QueuePayloadPrefixCapture {
    scope: QueueScope,
    input: QueuePayloadPrefixInput,
    body: Vec<u8>,
    hash: QueueSha256,
}

impl QueuePayloadPrefixCapture {
    pub fn new(
        scope: QueueScope,
        input: QueuePayloadPrefixInput,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        validate_item_ids(&input.item_ids)?;
        if !message_id(input.id.as_str()) {
            return Err(QueueManifestError::InvalidInput);
        }
        let mut value = json!({"format":"payload-v1","directory":scope.directory,"id":input.id.as_str(),
            "itemIds":input.item_ids.iter().map(QueuedItemId::as_str).collect::<Vec<_>>(),"composer":input.composer.value()});
        if input.steer {
            value["delivery"] = json!("steer");
        }
        Ok(Self {
            scope,
            input,
            body: encode(&value)?,
            hash,
        })
    }

    pub fn from_body(
        scope: QueueScope,
        body: Vec<u8>,
        hash: QueueSha256,
    ) -> Result<Self, QueueManifestError> {
        let wire: PayloadPrefixWire<PayloadPointerWire> = decode_input(&body)?;
        if wire.directory != scope.directory
            || wire.format != "payload-v1"
            || !matches!(wire.delivery.as_deref(), None | Some("steer"))
        {
            return Err(QueueManifestError::InvalidInput);
        }
        let composer = payload_pointer(wire.composer)?;
        let mut capture = Self::new(
            scope,
            QueuePayloadPrefixInput {
                id: MessageId::new(wire.id),
                item_ids: wire.item_ids.into_iter().map(QueuedItemId::new).collect(),
                composer,
                steer: wire.delivery.is_some(),
            },
            hash,
        )?;
        capture.body = body;
        Ok(capture)
    }

    pub fn scope(&self) -> &QueueScope {
        &self.scope
    }
    pub fn input(&self) -> &QueuePayloadPrefixInput {
        &self.input
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub async fn preview(&self) -> Result<QueuePayloadPrefix, QueueManifestError> {
        self.call("preview-prefix").await
    }
    pub async fn reserve(&self) -> Result<QueuePayloadPrefix, QueueManifestError> {
        self.call("reserve-prefix").await
    }

    async fn call(&self, operation: &str) -> Result<QueuePayloadPrefix, QueueManifestError> {
        let request = self
            .scope
            .authorized(Method::POST, self.scope.url(&[operation]))
            .header(CONTENT_TYPE.as_str(), "application/json")
            .body(self.body.clone());
        self.parse_reply(&json_body(send(request).await?).await?)
    }

    /// Parse a live or journal-restored complete compact handoff. The caller
    /// compares preview with reservation before admitting the saved prompt.
    pub fn parse_reply(&self, bytes: &[u8]) -> Result<QueuePayloadPrefix, QueueManifestError> {
        let wire: PayloadPrefixReplyWire = decode(bytes)?;
        let saved = wire.reservation;
        if saved.format != "payload-v1"
            || saved.directory != self.scope.directory
            || saved.id != self.input.id.as_str()
            || saved.item_ids.iter().map(String::as_str).ne(self
                .input
                .item_ids
                .iter()
                .map(QueuedItemId::as_str))
            || !matches!(saved.delivery.as_deref(), None | Some("steer"))
            || saved.delivery.is_some() != self.input.steer
        {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        let composer = indexed_payload(saved.composer, &self.scope.directory, self.hash)?;
        if composer.pointer != self.input.composer {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        let items = payload_taken_items(wire.items, &self.scope.directory, self.hash)?;
        if items
            .iter()
            .map(|item| &item.id)
            .ne(self.input.item_ids.iter())
        {
            return Err(QueueManifestError::ReceiptMismatch);
        }
        let context_ids = parse_message_ids(wire.context_ids)?;
        let expected: Vec<_> = items
            .iter()
            .flat_map(|item| item.payload.index.context_ids.iter())
            .chain(composer.index.context_ids.iter())
            .collect();
        let entries: usize = items
            .iter()
            .map(|item| item.payload.index.entry_count)
            .sum::<usize>()
            + composer.index.entry_count;
        let structured: u64 = items
            .iter()
            .map(|item| item.payload.index.structured_bytes)
            .sum::<u64>()
            + composer.index.structured_bytes;
        if expected.into_iter().ne(context_ids.iter())
            || context_ids.contains(&self.input.id)
            || entries > QUEUE_MANIFEST_ENTRIES
            || structured > QUEUE_PAYLOAD_BYTES as u64
        {
            return Err(QueueManifestError::InvalidResponse);
        }
        Ok(QueuePayloadPrefix {
            revision: safe_integer(wire.revision)?,
            directory: self.scope.directory.clone(),
            id: self.input.id.clone(),
            item_ids: self.input.item_ids.clone(),
            steer: self.input.steer,
            composer,
            items,
            context_ids,
        })
    }
}

impl fmt::Debug for QueuePayloadPrefixCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuePayloadPrefixCapture")
            .field("scope", &self.scope)
            .field("input", &self.input)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePayloadPrefix {
    pub revision: u64,
    pub directory: String,
    pub id: MessageId,
    pub item_ids: Vec<QueuedItemId>,
    pub steer: bool,
    pub composer: QueueIndexedPayload,
    pub items: Vec<QueuePayloadTakenItem>,
    pub context_ids: Vec<MessageId>,
}

// serde_json's byte-string visitor preserves JSON lone-surrogate escapes as
// WTF-8. Keep their UTF-16 units only for the backend's display-only preview.
// Other typed strings continue to require normal Unicode scalar strings.
/// Bounded display text retaining JavaScript's exact UTF-16 units, including a
/// surrogate split by a preview slice. Other queue text requires valid Unicode.
#[derive(Clone, PartialEq, Eq)]
pub struct PayloadPreview(Vec<u16>);

impl PayloadPreview {
    /// Captures ordinary Unicode text. The queue boundary checks the 103-unit limit.
    pub fn new(text: &str) -> Self {
        Self::from_str(text)
    }

    fn from_str(text: &str) -> Self {
        Self(text.encode_utf16().collect())
    }

    /// Returns display-only text with replacement characters for split surrogates.
    pub fn display(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }

    /// Returns the immutable units retained by the captured preview.
    pub fn utf16(&self) -> &[u16] {
        &self.0
    }

    fn from_wtf8(mut bytes: &[u8]) -> Option<Self> {
        let mut units = Vec::new();
        while !bytes.is_empty() {
            let valid = match std::str::from_utf8(bytes) {
                Ok(text) => text,
                Err(error) => std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()?,
            };
            if units.len() + valid.encode_utf16().count() > 103 {
                return None;
            }
            units.extend(valid.encode_utf16());
            bytes = &bytes[valid.len()..];
            if bytes.is_empty() {
                break;
            }
            if bytes.len() < 3
                || bytes[0] != 0xed
                || !(0xa0..=0xbf).contains(&bytes[1])
                || !(0x80..=0xbf).contains(&bytes[2])
                || units.len() == 103
            {
                return None;
            }
            units.push(0xd000 | (u16::from(bytes[1] & 0x3f) << 6) | u16::from(bytes[2] & 0x3f));
            bytes = &bytes[3..];
        }
        Some(Self(units))
    }

    fn append_json(&self, bytes: &mut Vec<u8>) -> Result<(), QueueManifestError> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        bytes.push(b'"');
        for decoded in char::decode_utf16(self.0.iter().copied()) {
            match decoded {
                Ok(character) => {
                    let escaped = serde_json::to_vec(&character)
                        .map_err(|_| QueueManifestError::InvalidResponse)?;
                    bytes.extend_from_slice(&escaped[1..escaped.len() - 1]);
                }
                Err(error) => {
                    let unit = error.unpaired_surrogate();
                    bytes.extend_from_slice(b"\\u");
                    for shift in [12, 8, 4, 0] {
                        bytes.push(HEX[((unit >> shift) & 15) as usize]);
                    }
                }
            }
        }
        bytes.push(b'"');
        Ok(())
    }
}

impl fmt::Debug for PayloadPreview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PayloadPreview")
            .field("units", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl From<&str> for PayloadPreview {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl From<String> for PayloadPreview {
    fn from(text: String) -> Self {
        Self::new(&text)
    }
}

impl<'de> Deserialize<'de> for PayloadPreview {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PreviewVisitor;
        impl serde::de::Visitor<'_> for PreviewVisitor {
            type Value = PayloadPreview;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a display preview of at most 103 UTF-16 units")
            }
            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
                PayloadPreview::from_wtf8(bytes).ok_or_else(|| E::custom("invalid display preview"))
            }
            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
                self.visit_bytes(text.as_bytes())
            }
            fn visit_string<E: serde::de::Error>(self, text: String) -> Result<Self::Value, E> {
                self.visit_str(&text)
            }
        }
        deserializer.deserialize_bytes(PreviewVisitor)
    }
}

// Private wire records. Only validated owned records above leave this boundary.
// Field order here is part of the backend's JSON.stringify(schema.parse(...))
// index digest. In particular blobDescriptorSchema starts with attachmentId.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PayloadChunkWire {
    attachment_id: String,
    filename: String,
    mime_type: String,
    size: u64,
    sha256: String,
    blob_id: String,
    request_id: String,
    ordinal: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PayloadManifestWire {
    version: u64,
    encoding: String,
    total_bytes: u64,
    sha256: String,
    chunks: Vec<PayloadChunkWire>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PayloadPointerWire {
    format: String,
    payload_manifest: PayloadManifestWire,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PayloadIndexWire {
    directory: String,
    send_config: ConfigWire,
    preview: PayloadPreview,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    context_preview: Option<PayloadPreview>,
    entry_count: usize,
    context_ids: Vec<String>,
    attachment_blob_ids: Vec<String>,
    prompt_bytes: u64,
    admission_bytes: u64,
    structured_bytes: u64,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    legacy_attachment_ids: Option<Vec<Option<String>>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IndexedPayloadWire {
    format: String,
    payload_manifest: PayloadManifestWire,
    payload_index: PayloadIndexWire,
    payload_index_hash: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PayloadTakenWire {
    id: String,
    created_at: u64,
    #[serde(default, deserialize_with = "optional")]
    request_id: Option<String>,
    format: String,
    payload_manifest: PayloadManifestWire,
    payload_index: PayloadIndexWire,
    payload_index_hash: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PayloadTakeReplyWire {
    receipt: TakeReceiptWire,
    state: String,
    revision: u64,
    #[serde(default, deserialize_with = "optional")]
    items: Option<Vec<PayloadTakenWire>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PayloadPrefixWire<C> {
    format: String,
    directory: String,
    id: String,
    item_ids: Vec<String>,
    composer: C,
    #[serde(default)]
    delivery: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PayloadPrefixReplyWire {
    revision: u64,
    reservation: PayloadPrefixWire<IndexedPayloadWire>,
    items: Vec<PayloadTakenWire>,
    context_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PayloadLegacyAttachmentWire {
    #[serde(default, deserialize_with = "optional")]
    id: Option<String>,
    filename: String,
    mime_type: String,
    #[serde(default = "zero_legacy_size")]
    size: serde_json::Number,
    #[serde(default = "local_attachment_source")]
    source: String,
    #[serde(default, deserialize_with = "optional")]
    server_path: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    agent_path: Option<String>,
    data_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PayloadLegacyItemWire {
    #[serde(default, deserialize_with = "optional")]
    content: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    text: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    agent_mention: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    context_preview: Option<PayloadPreview>,
    #[serde(default)]
    context: Vec<LegacyWire>,
    send_config: ConfigWire,
    #[serde(default)]
    attachments: Vec<PayloadLegacyAttachmentWire>,
    #[serde(default)]
    prompt_parts: LegacyReferencePartsWire,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyReferencePartsWire {
    #[serde(default)]
    file_references: Vec<FileReferenceWire>,
    #[serde(default)]
    agents: Vec<AgentWire>,
    #[serde(default)]
    skills: Vec<SkillWire>,
    #[serde(default)]
    context: Vec<PromptContextWire>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DescriptorWire {
    blob_id: String,
    request_id: String,
    attachment_id: String,
    filename: String,
    mime_type: String,
    size: u64,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachmentReplyWire {
    attachment: DescriptorWire,
    state: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MentionWire {
    start: u64,
    end: u64,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentWire {
    name: String,
    #[serde(default, deserialize_with = "optional")]
    mention: Option<MentionWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillWire {
    id: String,
    #[serde(default, deserialize_with = "optional")]
    mention: Option<MentionWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptContextWire {
    #[serde(default, deserialize_with = "optional")]
    id: Option<String>,
    text: String,
    #[serde(default, deserialize_with = "optional")]
    description: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    metadata: Option<Map<String, Value>>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartsWire {
    #[serde(default, rename = "fileReferences")]
    file_references: Vec<FileReferenceWire>,
    #[serde(default)]
    agents: Vec<AgentWire>,
    #[serde(default)]
    skills: Vec<SkillWire>,
    #[serde(default, deserialize_with = "optional")]
    metadata: Option<Map<String, Value>>,
    #[serde(default)]
    context: Vec<PromptContextWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileReferenceWire {
    uri: String,
    name: String,
    #[serde(default, deserialize_with = "optional")]
    description: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    mention: Option<MentionWire>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum LegacyWire {
    Context {
        text: String,
        metadata: Map<String, Value>,
        #[serde(default, deserialize_with = "optional")]
        instructions: Option<String>,
    },
    Instruction {
        text: String,
    },
    Synthetic {
        text: String,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigWire {
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(rename = "modelID")]
    model_id: String,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    agent: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    variant: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestAttachmentWire {
    blob_id: String,
    request_id: String,
    attachment_id: String,
    filename: String,
    mime_type: String,
    size: u64,
    sha256: String,
    #[serde(default, deserialize_with = "optional")]
    agent_path: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    description: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    mention: Option<MentionWire>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ItemWire {
    format: String,
    content: String,
    text: String,
    attachment_manifest: Vec<ManifestAttachmentWire>,
    #[serde(default)]
    context: Vec<LegacyWire>,
    #[serde(default, deserialize_with = "optional")]
    context_preview: Option<PayloadPreview>,
    send_config: ConfigWire,
    #[serde(default)]
    prompt_parts: PartsWire,
    #[serde(default, deserialize_with = "optional")]
    id: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    created_at: Option<u64>,
    #[serde(default, deserialize_with = "optional")]
    request_id: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    attachments: Option<Vec<Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EnqueueReceiptWire {
    version: u64,
    kind: String,
    request_id: String,
    session_id: String,
    directory: String,
    request_hash: String,
    accepted_at: u64,
    item_id: String,
    message_id: String,
    context_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TakeReceiptWire {
    version: u64,
    kind: String,
    request_id: String,
    session_id: String,
    directory: String,
    request_hash: String,
    accepted_at: u64,
    item_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnqueueReplyWire {
    receipt: EnqueueReceiptWire,
    state: QueueEnqueueState,
    revision: u64,
    attempt: u64,
    #[serde(default, deserialize_with = "optional")]
    blocked: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TakeReplyWire {
    receipt: TakeReceiptWire,
    state: String,
    revision: u64,
    #[serde(default, deserialize_with = "optional")]
    items: Option<Vec<ItemWire>>,
}

enum RecordWire {
    Enqueue(EnqueueReplyWire),
    Take(TakeReplyWire),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PrefixWire {
    format: String,
    directory: String,
    id: String,
    item_ids: Vec<String>,
    composer: String,
    send_config: ConfigWire,
    #[serde(default)]
    delivery: Option<String>,
    #[serde(default, deserialize_with = "optional")]
    prompt_parts: Option<PartsWire>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PrefixReplyWire {
    revision: u64,
    reservation: PrefixWire,
    items: Vec<ItemWire>,
    context_ids: Vec<String>,
}

// Missing optional fields are allowed; explicit null is not part of these schemas.
fn optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn payload_manifest_wire(pointer: &QueuePayloadPointer) -> PayloadManifestWire {
    PayloadManifestWire {
        version: 1,
        encoding: "utf-8-json".to_owned(),
        total_bytes: pointer.total_bytes,
        sha256: hex_digest(&pointer.sha256),
        chunks: pointer
            .chunks
            .iter()
            .map(|chunk| {
                let d = &chunk.attachment;
                PayloadChunkWire {
                    attachment_id: d.attachment_id.clone(),
                    filename: d.filename.clone(),
                    mime_type: d.mime_type.clone(),
                    size: d.size,
                    sha256: hex_digest(&d.sha256),
                    blob_id: hex_digest(&d.blob_id),
                    request_id: d.request_id.clone(),
                    ordinal: chunk.ordinal,
                }
            })
            .collect(),
    }
}

fn payload_pointer(wire: PayloadPointerWire) -> Result<QueuePayloadPointer, QueueManifestError> {
    let manifest = wire.payload_manifest;
    if wire.format != "payload-v1" || manifest.version != 1 || manifest.encoding != "utf-8-json" {
        return Err(QueueManifestError::InvalidResponse);
    }
    let sha256 = parse_digest(&manifest.sha256).ok_or(QueueManifestError::InvalidResponse)?;
    let chunks = manifest
        .chunks
        .into_iter()
        .map(|chunk| {
            Ok(QueuePayloadChunk {
                ordinal: chunk.ordinal,
                attachment: descriptor(DescriptorWire {
                    blob_id: chunk.blob_id,
                    request_id: chunk.request_id,
                    attachment_id: chunk.attachment_id,
                    filename: chunk.filename,
                    mime_type: chunk.mime_type,
                    size: chunk.size,
                    sha256: chunk.sha256,
                })?,
            })
        })
        .collect::<Result<_, QueueManifestError>>()?;
    QueuePayloadPointer::new(manifest.total_bytes, sha256, chunks)
}

fn payload_index_wire(index: &QueuePayloadIndex) -> PayloadIndexWire {
    PayloadIndexWire {
        directory: index.directory.clone(),
        send_config: ConfigWire {
            provider_id: index.send_config.provider_id.clone(),
            model_id: index.send_config.model_id.clone(),
            agent: index.send_config.agent.clone(),
            variant: index.send_config.variant.clone(),
        },
        preview: PayloadPreview::from_str(&index.preview),
        context_preview: index
            .context_preview
            .as_deref()
            .map(PayloadPreview::from_str),
        entry_count: index.entry_count,
        context_ids: index
            .context_ids
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect(),
        attachment_blob_ids: index.attachment_blob_ids.iter().map(hex_digest).collect(),
        prompt_bytes: index.prompt_bytes,
        admission_bytes: index.admission_bytes,
        structured_bytes: index.structured_bytes,
        legacy_attachment_ids: index.legacy_attachment_ids.clone(),
    }
}

fn append_json<T: Serialize + ?Sized>(
    bytes: &mut Vec<u8>,
    value: &T,
) -> Result<(), QueueManifestError> {
    serde_json::to_writer(bytes, value).map_err(|_| QueueManifestError::InvalidResponse)
}

fn payload_index_bytes(
    index: &QueuePayloadIndex,
    preview: &PayloadPreview,
    context_preview: Option<&PayloadPreview>,
) -> Result<Vec<u8>, QueueManifestError> {
    // This is the backend indexSchema order. The preview alone needs a UTF-16
    // writer because serde String/Value cannot represent escaped lone surrogates.
    let wire = payload_index_wire(index);
    let mut bytes = b"{\"directory\":".to_vec();
    append_json(&mut bytes, &wire.directory)?;
    bytes.extend_from_slice(b",\"sendConfig\":");
    append_json(&mut bytes, &wire.send_config)?;
    bytes.extend_from_slice(b",\"preview\":");
    preview.append_json(&mut bytes)?;
    if let Some(value) = context_preview {
        bytes.extend_from_slice(b",\"contextPreview\":");
        value.append_json(&mut bytes)?;
    }
    bytes.extend_from_slice(b",\"entryCount\":");
    append_json(&mut bytes, &wire.entry_count)?;
    bytes.extend_from_slice(b",\"contextIds\":");
    append_json(&mut bytes, &wire.context_ids)?;
    bytes.extend_from_slice(b",\"attachmentBlobIds\":");
    append_json(&mut bytes, &wire.attachment_blob_ids)?;
    bytes.extend_from_slice(b",\"promptBytes\":");
    append_json(&mut bytes, &wire.prompt_bytes)?;
    bytes.extend_from_slice(b",\"admissionBytes\":");
    append_json(&mut bytes, &wire.admission_bytes)?;
    bytes.extend_from_slice(b",\"structuredBytes\":");
    append_json(&mut bytes, &wire.structured_bytes)?;
    if let Some(value) = &wire.legacy_attachment_ids {
        bytes.extend_from_slice(b",\"legacyAttachmentIds\":");
        append_json(&mut bytes, value)?;
    }
    bytes.push(b'}');
    Ok(bytes)
}

fn payload_binding_bytes(
    pointer: &QueuePayloadPointer,
    index: &QueuePayloadIndex,
    preview: &PayloadPreview,
    context_preview: Option<&PayloadPreview>,
) -> Result<Vec<u8>, QueueManifestError> {
    let mut bytes = b"{\"payloadManifest\":".to_vec();
    append_json(&mut bytes, &payload_manifest_wire(pointer))?;
    bytes.extend_from_slice(b",\"payloadIndex\":");
    bytes.extend_from_slice(&payload_index_bytes(index, preview, context_preview)?);
    bytes.push(b'}');
    Ok(bytes)
}

fn indexed_payload(
    wire: IndexedPayloadWire,
    directory: &str,
    hash: QueueSha256,
) -> Result<QueueIndexedPayload, QueueManifestError> {
    let pointer = payload_pointer(PayloadPointerWire {
        format: wire.format,
        payload_manifest: wire.payload_manifest,
    })?;
    let raw = wire.payload_index;
    let index_hash =
        parse_digest(&wire.payload_index_hash).ok_or(QueueManifestError::InvalidResponse)?;
    let context_ids = parse_message_ids(raw.context_ids)?;
    let roots = raw
        .attachment_blob_ids
        .iter()
        .map(|id| parse_digest(id).ok_or(QueueManifestError::InvalidResponse))
        .collect::<Result<Vec<_>, _>>()?;
    if raw.directory != directory || !valid_payload_directory(directory) {
        return Err(QueueManifestError::ReceiptMismatch);
    }
    if raw
        .context_preview
        .as_ref()
        .is_some_and(|v| v.0.len() > 103)
        || raw.entry_count > QUEUE_MANIFEST_ENTRIES
        || roots.len() > QUEUE_MANIFEST_ENTRIES
        || roots.iter().collect::<HashSet<_>>().len() != roots.len()
        || raw.structured_bytes > QUEUE_PAYLOAD_BYTES as u64
        || raw.prompt_bytes > MAX_SAFE_INTEGER
        || raw.admission_bytes > MAX_SAFE_INTEGER
        || raw.admission_bytes < raw.prompt_bytes
        || raw.legacy_attachment_ids.as_ref().is_some_and(|ids| {
            ids.len() > QUEUE_MANIFEST_ENTRIES
                || ids
                    .iter()
                    .flatten()
                    .any(|id| id.is_empty() || js_len(id) > 256)
        })
        || raw.entry_count
            < context_ids.len()
                + roots.len()
                + raw.legacy_attachment_ids.as_ref().map_or(0, Vec::len)
    {
        return Err(QueueManifestError::InvalidResponse);
    }
    let preview = raw.preview;
    let context_preview = raw.context_preview;
    let index = QueuePayloadIndex {
        directory: raw.directory,
        send_config: config(raw.send_config)?,
        preview: preview.display(),
        context_preview: context_preview.as_ref().map(PayloadPreview::display),
        entry_count: raw.entry_count,
        context_ids,
        attachment_blob_ids: roots,
        prompt_bytes: raw.prompt_bytes,
        admission_bytes: raw.admission_bytes,
        structured_bytes: raw.structured_bytes,
        legacy_attachment_ids: raw.legacy_attachment_ids,
    };
    let bytes = payload_binding_bytes(&pointer, &index, &preview, context_preview.as_ref())?;
    if hash(&bytes) != index_hash {
        return Err(QueueManifestError::PayloadMismatch);
    }
    let result = QueueIndexedPayload {
        pointer,
        index,
        index_hash,
        preview,
        context_preview,
    };
    result.body()?;
    Ok(result)
}

fn payload_taken_items(
    wires: Vec<PayloadTakenWire>,
    directory: &str,
    hash: QueueSha256,
) -> Result<Vec<QueuePayloadTakenItem>, QueueManifestError> {
    if wires.is_empty() || wires.len() > 20 {
        return Err(QueueManifestError::InvalidResponse);
    }
    let mut ids = HashSet::new();
    let mut items = Vec::with_capacity(wires.len());
    for wire in wires {
        if !valid_item_id(&wire.id)
            || !ids.insert(wire.id.clone())
            || wire
                .request_id
                .as_deref()
                .is_some_and(|id| !ascii_id(id, 8, 128))
        {
            return Err(QueueManifestError::InvalidResponse);
        }
        items.push(QueuePayloadTakenItem {
            id: QueuedItemId::new(wire.id),
            created_at: safe_integer(wire.created_at)?,
            request_id: wire.request_id,
            payload: indexed_payload(
                IndexedPayloadWire {
                    format: wire.format,
                    payload_manifest: wire.payload_manifest,
                    payload_index: wire.payload_index,
                    payload_index_hash: wire.payload_index_hash,
                },
                directory,
                hash,
            )?,
        });
    }
    if items
        .iter()
        .map(|item| item.payload.index.entry_count)
        .sum::<usize>()
        > QUEUE_MANIFEST_ENTRIES
    {
        return Err(QueueManifestError::InvalidResponse);
    }
    Ok(items)
}

fn valid_payload_directory(directory: &str) -> bool {
    !directory.is_empty() && js_len(directory) <= 16384 && js_trim(directory) == directory
}

fn payload_preview_units(content: &str, text: &str) -> PayloadPreview {
    let text = if content.is_empty() { text } else { content };
    let mut units: Vec<_> = text.encode_utf16().take(100).collect();
    if js_len(text) > 100 {
        units.extend([46, 46, 46]);
    }
    PayloadPreview(units)
}

fn parse_payload_item(
    value: Value,
    usage: QueuePayloadUse,
) -> Result<QueuePayloadItem, QueueManifestError> {
    let allow_empty = usage == QueuePayloadUse::Composer;
    if value.get("format").is_some() {
        let wire: ItemWire =
            serde_json::from_value(value).map_err(|_| QueueManifestError::InvalidInput)?;
        if wire.id.is_some()
            || wire.created_at.is_some()
            || wire.request_id.is_some()
            || wire.attachments.is_some()
        {
            return Err(QueueManifestError::InvalidInput);
        }
        return manifest_item_for(wire, MetadataOrigin::JavaScript, allow_empty)
            .map(QueuePayloadItem::Manifest);
    }
    let raw: PayloadLegacyItemWire =
        serde_json::from_value(value).map_err(|_| QueueManifestError::InvalidInput)?;
    if !raw.prompt_parts.agents.is_empty()
        || !raw.prompt_parts.skills.is_empty()
        || !raw.prompt_parts.context.is_empty()
    {
        return Err(QueueManifestError::InvalidInput);
    }
    let file_references = file_references(raw.prompt_parts.file_references)?;
    let content = raw.content.unwrap_or_default();
    let text = raw.text.unwrap_or_else(|| content.clone());
    if js_len(&content) > 200_000
        || raw
            .context_preview
            .as_ref()
            .is_some_and(|v| v.0.len() > 103)
        || raw.agent_mention.as_deref().is_some_and(|v| !identifier(v))
        || raw.context.len() > QUEUE_MANIFEST_ENTRIES
        || raw.attachments.len() > QUEUE_MANIFEST_ENTRIES
    {
        return Err(QueueManifestError::InvalidInput);
    }
    let mut context = Vec::with_capacity(raw.context.len());
    for entry in raw.context {
        let entry = match entry {
            LegacyWire::Context {
                text,
                metadata,
                instructions,
            } => {
                let metadata = metadata_from_wire(metadata, MetadataOrigin::JavaScript)?;
                validate_metadata(&metadata)?;
                if instructions
                    .as_deref()
                    .is_some_and(|s| js_trim(s).is_empty())
                {
                    return Err(QueueManifestError::InvalidInput);
                }
                QueueLegacyContext::Context {
                    text,
                    metadata,
                    instructions,
                }
            }
            LegacyWire::Instruction { text } => QueueLegacyContext::Instruction { text },
            LegacyWire::Synthetic { text } => QueueLegacyContext::Synthetic { text },
        };
        let text = match &entry {
            QueueLegacyContext::Context { text, .. }
            | QueueLegacyContext::Instruction { text }
            | QueueLegacyContext::Synthetic { text } => text,
        };
        if js_trim(text).is_empty() {
            return Err(QueueManifestError::InvalidInput);
        }
        context.push(entry);
    }
    if raw.attachments.len()
        + file_references.len()
        + usize::from(raw.agent_mention.is_some())
        + legacy_count(&context)
        > QUEUE_MANIFEST_ENTRIES
        || (!allow_empty
            && js_trim(&text).is_empty()
            && raw.attachments.is_empty()
            && file_references.is_empty()
            && context.is_empty())
    {
        return Err(QueueManifestError::InvalidInput);
    }
    let mut attachments = Vec::with_capacity(raw.attachments.len());
    for file in raw.attachments {
        if file.id.as_deref().is_some_and(str::is_empty)
            || file.filename.is_empty()
            || file.mime_type.is_empty()
            || file
                .size
                .as_f64()
                .is_none_or(|size| size < 0.0 || size.fract() != 0.0)
            || file.server_path.as_deref().is_some_and(str::is_empty)
            || file.agent_path.as_deref().is_some_and(str::is_empty)
            || !matches!(file.source.as_str(), "local" | "server" | "vscode")
        {
            return Err(QueueManifestError::InvalidInput);
        }
        let size = file
            .size
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .ok_or(QueueManifestError::InvalidInput)?;
        let file = QueuePayloadLegacyAttachment {
            id: file.id,
            filename: file.filename,
            mime_type: file.mime_type,
            size,
            source: file.source,
            server_path: file.server_path,
            agent_path: file.agent_path,
            data_url: file.data_url,
        };
        file.bytes()?;
        attachments.push(file);
    }
    Ok(QueuePayloadItem::Legacy(QueuePayloadLegacyItem {
        content,
        text,
        agent_mention: raw.agent_mention,
        context_preview: raw.context_preview,
        context,
        send_config: config(raw.send_config)?,
        attachments,
        file_references,
    }))
}

fn local_attachment_source() -> String {
    "local".to_owned()
}

fn zero_legacy_size() -> serde_json::Number {
    0.into()
}

// Captures use encodeLegacyCapture's shortest decimal/exponential spelling.
// Outgoing requests use JSON.stringify's fixed/scientific notation thresholds.
// Both preserve the already-validated JavaScript Number and normalize -0 to 0.
struct QueueNumberFormatter {
    compact: bool,
}

/// Write validated queue request values with the pinned SDK's JSON.stringify
/// number spelling. Call the same writer for size counting and byte emission.
/// Metadata must already have passed the typed capture's numeric validation;
/// this serializer performs no parsing or integer-to-JavaScript normalization.
pub(crate) fn write_queue_request_json<W: io::Write, T: Serialize + ?Sized>(
    writer: W,
    value: &T,
) -> Result<(), serde_json::Error> {
    value.serialize(&mut serde_json::Serializer::with_formatter(
        writer,
        QueueNumberFormatter { compact: false },
    ))
}

impl serde_json::ser::Formatter for QueueNumberFormatter {
    fn write_f64<W: io::Write + ?Sized>(&mut self, writer: &mut W, value: f64) -> io::Result<()> {
        if value == 0.0 {
            return writer.write_all(b"0");
        }
        if self.compact {
            let decimal = value.to_string();
            let exponential = format!("{value:e}");
            let shortest = if exponential.len() < decimal.len() {
                &exponential
            } else {
                &decimal
            };
            return writer.write_all(shortest.as_bytes());
        }
        write_request_f64(writer, value)
    }

    fn write_i64<W: io::Write + ?Sized>(&mut self, writer: &mut W, value: i64) -> io::Result<()> {
        if self.compact {
            self.write_f64(writer, value as f64)
        } else {
            write!(writer, "{value}")
        }
    }

    fn write_u64<W: io::Write + ?Sized>(&mut self, writer: &mut W, value: u64) -> io::Result<()> {
        if self.compact {
            self.write_f64(writer, value as f64)
        } else {
            write!(writer, "{value}")
        }
    }
}

fn write_request_f64<W: io::Write + ?Sized>(writer: &mut W, value: f64) -> io::Result<()> {
    // Use serde_json's supported formatter for the shortest nearest-even digits.
    // Its pinned zmij converter covers the 2^-25 midpoint where Rust Display's
    // tie choice differs from ECMAScript. Only decimal-point placement changes.
    let mut raw = Vec::with_capacity(24);
    serde_json::ser::Formatter::write_f64(&mut serde_json::ser::CompactFormatter, &mut raw, value)?;
    let raw = std::str::from_utf8(&raw).expect("the JSON float formatter emits ASCII");
    let (negative, raw) = raw
        .strip_prefix('-')
        .map_or((false, raw), |raw| (true, raw));
    let (mantissa, exponent) = raw
        .split_once('e')
        .map_or((raw, 0), |(mantissa, exponent)| {
            (
                mantissa,
                exponent
                    .parse::<i32>()
                    .expect("the JSON formatter emits an integer exponent"),
            )
        });
    let before_point = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let mut digits: Vec<u8> = mantissa.bytes().filter(|byte| *byte != b'.').collect();
    let leading = digits.iter().take_while(|digit| **digit == b'0').count();
    let point = before_point + exponent - leading as i32;
    drop(digits.drain(..leading));
    while digits.last() == Some(&b'0') {
        digits.pop();
    }
    if negative {
        writer.write_all(b"-")?;
    }
    if point > 0 && point <= 21 {
        let split = (point as usize).min(digits.len());
        writer.write_all(&digits[..split])?;
        if split < digits.len() {
            writer.write_all(b".")?;
            writer.write_all(&digits[split..])?;
        } else {
            for _ in split..point as usize {
                writer.write_all(b"0")?;
            }
        }
    } else if point <= 0 && point > -6 {
        writer.write_all(b"0.")?;
        for _ in 0..-point {
            writer.write_all(b"0")?;
        }
        writer.write_all(&digits)?;
    } else {
        writer.write_all(&digits[..1])?;
        if digits.len() > 1 {
            writer.write_all(b".")?;
            writer.write_all(&digits[1..])?;
        }
        write!(writer, "e{:+}", point - 1)?;
    }
    Ok(())
}

pub(crate) fn compact_queue_json_len<T: Serialize + ?Sized>(
    value: &T,
) -> Result<usize, QueueManifestError> {
    let mut count = PayloadByteCount(0);
    value
        .serialize(&mut serde_json::Serializer::with_formatter(
            &mut count,
            QueueNumberFormatter { compact: true },
        ))
        .map_err(|error| {
            if error.is_io() {
                QueueManifestError::PayloadTooLarge
            } else {
                QueueManifestError::InvalidInput
            }
        })?;
    Ok(count.0)
}

struct PayloadByteCount(usize);

impl io::Write for PayloadByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > QUEUE_PAYLOAD_BYTES - self.0 {
            return Err(io::Error::other("structured capture exceeds its limit"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum PayloadEncoding {
    Manifest,
    Legacy,
}

impl PayloadEncoding {
    fn write<W: io::Write>(&self, writer: W, value: &Value) -> Result<(), serde_json::Error> {
        match self {
            Self::Manifest => serde_json::to_writer(writer, value),
            Self::Legacy => value.serialize(&mut serde_json::Serializer::with_formatter(
                writer,
                QueueNumberFormatter { compact: true },
            )),
        }
    }
}

fn encode_payload_capture(
    directory: &str,
    mut item: Value,
    preview: Option<&PayloadPreview>,
    encoding: PayloadEncoding,
) -> Result<Vec<u8>, QueueManifestError> {
    item.as_object_mut()
        .ok_or(QueueManifestError::InvalidInput)?
        .remove("contextPreview");
    let value = json!({"directory":directory,"item":item});
    let preview = preview
        .map(|preview| {
            if preview.0.len() > 103 {
                return Err(QueueManifestError::InvalidInput);
            }
            let mut bytes = b",\"contextPreview\":".to_vec();
            preview.append_json(&mut bytes)?;
            Ok(bytes)
        })
        .transpose()?;
    let mut count = PayloadByteCount(preview.as_ref().map_or(0, Vec::len));
    encoding.write(&mut count, &value).map_err(|error| {
        if error.is_io() {
            QueueManifestError::PayloadTooLarge
        } else {
            QueueManifestError::InvalidInput
        }
    })?;
    // Preflight the complete encoded length before allocating the capture.
    let mut bytes = Vec::with_capacity(count.0);
    encoding
        .write(&mut bytes, &value)
        .map_err(|_| QueueManifestError::InvalidInput)?;
    if let Some(preview) = preview {
        if !bytes.ends_with(b"}}") {
            return Err(QueueManifestError::InvalidInput);
        }
        bytes.truncate(bytes.len() - 2);
        bytes.extend_from_slice(&preview);
        bytes.extend_from_slice(b"}}");
    }
    Ok(bytes)
}

#[cfg(test)]
fn encode_legacy_payload(value: &Value) -> Result<Vec<u8>, QueueManifestError> {
    let mut count = PayloadByteCount(0);
    PayloadEncoding::Legacy
        .write(&mut count, value)
        .map_err(|error| {
            if error.is_io() {
                QueueManifestError::PayloadTooLarge
            } else {
                QueueManifestError::InvalidInput
            }
        })?;
    let mut bytes = Vec::with_capacity(count.0);
    PayloadEncoding::Legacy
        .write(&mut bytes, value)
        .map_err(|_| QueueManifestError::InvalidInput)?;
    Ok(bytes)
}

fn inline_payload_bytes(url: &str) -> Result<Vec<u8>, QueueManifestError> {
    let (header, encoded) = url
        .strip_prefix("data:")
        .and_then(|s| s.split_once(','))
        .ok_or(QueueManifestError::InvalidInput)?;
    if header.ends_with(";base64") {
        if encoded.len() > QUEUE_BLOB_BYTES.div_ceil(3) * 4 {
            return Err(QueueManifestError::BlobTooLarge);
        }
        if !encoded.len().is_multiple_of(4) {
            return Err(QueueManifestError::InvalidInput);
        }
        let digit = |b| match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        };
        let mut bytes = Vec::with_capacity(encoded.len() / 4 * 3);
        for (ordinal, part) in encoded.as_bytes().chunks_exact(4).enumerate() {
            let a = digit(part[0]).ok_or(QueueManifestError::InvalidInput)?;
            let b = digit(part[1]).ok_or(QueueManifestError::InvalidInput)?;
            let c = if part[2] == b'=' {
                0
            } else {
                digit(part[2]).ok_or(QueueManifestError::InvalidInput)?
            };
            let d = if part[3] == b'=' {
                0
            } else {
                digit(part[3]).ok_or(QueueManifestError::InvalidInput)?
            };
            if (part[2] == b'=' && (part[3] != b'=' || b & 15 != 0))
                || (part[3] == b'=' && part[2] != b'=' && c & 3 != 0)
                || ((part[2] == b'=' || part[3] == b'=') && ordinal + 1 != encoded.len() / 4)
            {
                return Err(QueueManifestError::InvalidInput);
            }
            bytes.push(a << 2 | b >> 4);
            if part[2] != b'=' {
                bytes.push(b << 4 | c >> 2);
            }
            if part[3] != b'=' {
                bytes.push(c << 6 | d);
            }
        }
        if bytes.len() > QUEUE_BLOB_BYTES {
            return Err(QueueManifestError::BlobTooLarge);
        }
        return Ok(bytes);
    }
    // decodeURIComponent preserves '+' and rejects malformed escapes/UTF-8.
    let mut bytes = Vec::new();
    let mut iter = encoded.bytes();
    while let Some(byte) = iter.next() {
        let byte = if byte == b'%' {
            let hi = iter
                .next()
                .and_then(|b| (b as char).to_digit(16))
                .ok_or(QueueManifestError::InvalidInput)?;
            let lo = iter
                .next()
                .and_then(|b| (b as char).to_digit(16))
                .ok_or(QueueManifestError::InvalidInput)?;
            (hi * 16 + lo) as u8
        } else {
            byte
        };
        if bytes.len() == QUEUE_BLOB_BYTES {
            return Err(QueueManifestError::BlobTooLarge);
        }
        bytes.push(byte);
    }
    std::str::from_utf8(&bytes).map_err(|_| QueueManifestError::InvalidInput)?;
    Ok(bytes)
}

fn descriptor(wire: DescriptorWire) -> Result<QueueAttachmentDescriptor, QueueManifestError> {
    let descriptor = QueueAttachmentDescriptor {
        blob_id: parse_digest(&wire.blob_id).ok_or(QueueManifestError::InvalidResponse)?,
        sha256: parse_digest(&wire.sha256).ok_or(QueueManifestError::InvalidResponse)?,
        request_id: wire.request_id,
        attachment_id: wire.attachment_id,
        filename: wire.filename,
        mime_type: wire.mime_type,
        size: wire.size,
    };
    validate_descriptor(&descriptor).map_err(|_| QueueManifestError::InvalidResponse)?;
    Ok(descriptor)
}

fn mention(wire: MentionWire) -> Result<QueueMention, QueueManifestError> {
    let mention = QueueMention {
        start: wire.start,
        end: wire.end,
        text: wire.text,
    };
    validate_mention(&mention).map_err(|_| QueueManifestError::InvalidResponse)?;
    Ok(mention)
}

#[derive(Clone, Copy)]
enum MetadataOrigin {
    Captured,
    JavaScript,
}

fn parts(wire: PartsWire, origin: MetadataOrigin) -> Result<QueuePromptParts, QueueManifestError> {
    let parts = QueuePromptParts {
        file_references: file_references(wire.file_references)?,
        agents: wire
            .agents
            .into_iter()
            .map(|agent| {
                Ok(QueueAgent {
                    name: agent.name,
                    mention: agent.mention.map(mention).transpose()?,
                })
            })
            .collect::<Result<_, QueueManifestError>>()?,
        skills: wire
            .skills
            .into_iter()
            .map(|skill| {
                Ok(QueueSkill {
                    id: skill.id,
                    mention: skill.mention.map(mention).transpose()?,
                })
            })
            .collect::<Result<_, QueueManifestError>>()?,
        metadata: wire
            .metadata
            .map(|metadata| metadata_from_wire(metadata, origin))
            .transpose()?,
        context: wire
            .context
            .into_iter()
            .map(|context| {
                Ok(QueuePromptContext {
                    id: context.id.map(MessageId::new),
                    text: context.text,
                    description: context.description,
                    metadata: context
                        .metadata
                        .map(|metadata| metadata_from_wire(metadata, origin))
                        .transpose()?,
                })
            })
            .collect::<Result<_, QueueManifestError>>()?,
    };
    validate_parts(&parts).map_err(|_| QueueManifestError::InvalidResponse)?;
    Ok(parts)
}

fn config(wire: ConfigWire) -> Result<QueuedSendConfig, QueueManifestError> {
    let config = QueuedSendConfig {
        provider_id: wire.provider_id,
        model_id: wire.model_id,
        agent: wire.agent,
        variant: wire.variant,
    };
    validate_config(&config).map_err(|_| QueueManifestError::InvalidResponse)?;
    Ok(config)
}

fn manifest_item(
    wire: ItemWire,
    origin: MetadataOrigin,
) -> Result<QueueManifestItem, QueueManifestError> {
    manifest_item_for(wire, origin, false)
}

fn manifest_item_for(
    wire: ItemWire,
    origin: MetadataOrigin,
    allow_empty: bool,
) -> Result<QueueManifestItem, QueueManifestError> {
    if wire.format != "manifest-v1" {
        return Err(QueueManifestError::InvalidResponse);
    }
    let files = wire
        .attachment_manifest
        .into_iter()
        .map(|file| {
            Ok(QueueManifestAttachment {
                attachment: descriptor(DescriptorWire {
                    blob_id: file.blob_id,
                    request_id: file.request_id,
                    attachment_id: file.attachment_id,
                    filename: file.filename,
                    mime_type: file.mime_type,
                    size: file.size,
                    sha256: file.sha256,
                })?,
                agent_path: file.agent_path,
                description: file.description,
                mention: file.mention.map(mention).transpose()?,
            })
        })
        .collect::<Result<_, QueueManifestError>>()?;
    let context = wire
        .context
        .into_iter()
        .map(|context| {
            Ok(match context {
                LegacyWire::Context {
                    text,
                    metadata,
                    instructions,
                } => QueueLegacyContext::Context {
                    text,
                    metadata: metadata_from_wire(metadata, origin)?,
                    instructions,
                },
                LegacyWire::Instruction { text } => QueueLegacyContext::Instruction { text },
                LegacyWire::Synthetic { text } => QueueLegacyContext::Synthetic { text },
            })
        })
        .collect::<Result<_, QueueManifestError>>()?;
    let item = QueueManifestItem {
        content: wire.content,
        text: wire.text,
        attachment_manifest: files,
        context,
        context_preview: wire.context_preview,
        send_config: config(wire.send_config)?,
        prompt_parts: parts(wire.prompt_parts, origin)?,
    };
    validate_item_for(&item, allow_empty).map_err(|_| QueueManifestError::InvalidResponse)?;
    Ok(item)
}

fn input_item(wire: ItemWire) -> Result<QueueManifestItem, QueueManifestError> {
    if wire.id.is_some()
        || wire.created_at.is_some()
        || wire.request_id.is_some()
        || wire.attachments.is_some()
    {
        return Err(QueueManifestError::InvalidInput);
    }
    manifest_item(wire, MetadataOrigin::Captured)
}

fn taken_items(wires: Vec<ItemWire>) -> Result<Vec<QueueTakenItem>, QueueManifestError> {
    if wires.is_empty() || wires.len() > 20 {
        return Err(QueueManifestError::InvalidResponse);
    }
    let mut ids = HashSet::new();
    let mut items = Vec::with_capacity(wires.len());
    for mut wire in wires {
        let id = wire.id.take().ok_or(QueueManifestError::InvalidResponse)?;
        if !valid_item_id(&id)
            || !ids.insert(id.clone())
            || wire
                .attachments
                .as_ref()
                .is_none_or(|attachments| !attachments.is_empty())
            || wire
                .request_id
                .as_deref()
                .is_some_and(|id| !ascii_id(id, 8, 128))
        {
            return Err(QueueManifestError::InvalidResponse);
        }
        items.push(QueueTakenItem {
            id: QueuedItemId::new(id),
            created_at: safe_integer(
                wire.created_at
                    .take()
                    .ok_or(QueueManifestError::InvalidResponse)?,
            )?,
            request_id: wire.request_id.take(),
            item: manifest_item(wire, MetadataOrigin::JavaScript)?,
        });
    }
    if total_entries(&items) > QUEUE_MANIFEST_ENTRIES {
        return Err(QueueManifestError::InvalidResponse);
    }
    Ok(items)
}

fn parse_record(
    bytes: &[u8],
    owner: &QueueRequestOwner,
) -> Result<QueueRecord, QueueManifestError> {
    // Decode each strict schema directly. Untagged Serde buffering would reject
    // split UTF-16 previews before their byte-string visitor can preserve them.
    let wire = match decode::<EnqueueReplyWire>(bytes) {
        Ok(wire) => RecordWire::Enqueue(wire),
        Err(_) => RecordWire::Take(decode(bytes)?),
    };
    match wire {
        RecordWire::Enqueue(wire) => {
            let raw = wire.receipt;
            if raw.version != 1 || raw.kind != "enqueue" {
                return Err(QueueManifestError::InvalidResponse);
            }
            let receipt = QueueEnqueueReceipt {
                identity: receipt_identity(
                    raw.request_id,
                    raw.session_id,
                    raw.directory,
                    raw.request_hash,
                    raw.accepted_at,
                )?,
                item_id: QueuedItemId::new(raw.item_id),
                message_id: MessageId::new(raw.message_id),
                context_ids: parse_message_ids(raw.context_ids)?,
            };
            validate_receipt_owner(&receipt.identity, owner)?;
            validate_enqueue_receipt(&receipt).map_err(|_| QueueManifestError::InvalidResponse)?;
            let blocked = match wire.blocked.as_deref() {
                None => None,
                Some("command_identity_unsupported") => {
                    Some(QueueBlocked::CommandIdentityUnsupported)
                }
                Some("prompt_body_too_large") => Some(QueueBlocked::PromptBodyTooLarge),
                _ => return Err(QueueManifestError::InvalidResponse),
            };
            Ok(QueueRecord::Enqueue(QueueEnqueueRecord {
                receipt,
                state: wire.state,
                revision: safe_integer(wire.revision)?,
                attempt: safe_integer(wire.attempt)?,
                blocked,
            }))
        }
        RecordWire::Take(wire) => {
            let raw = wire.receipt;
            if raw.version != 1 || raw.kind != "take" {
                return Err(QueueManifestError::InvalidResponse);
            }
            let receipt = QueueTakeReceipt {
                identity: receipt_identity(
                    raw.request_id,
                    raw.session_id,
                    raw.directory,
                    raw.request_hash,
                    raw.accepted_at,
                )?,
                item_ids: raw.item_ids.into_iter().map(QueuedItemId::new).collect(),
            };
            validate_receipt_owner(&receipt.identity, owner)?;
            validate_item_ids(&receipt.item_ids)
                .map_err(|_| QueueManifestError::InvalidResponse)?;
            let state = match (wire.state.as_str(), wire.items) {
                ("acknowledged", None) => QueueTakeState::Acknowledged,
                ("transferring", Some(items)) => {
                    let items = taken_items(items)?;
                    if items
                        .iter()
                        .map(|item| &item.id)
                        .ne(receipt.item_ids.iter())
                    {
                        return Err(QueueManifestError::ReceiptMismatch);
                    }
                    QueueTakeState::Transferring { items }
                }
                _ => return Err(QueueManifestError::InvalidResponse),
            };
            Ok(QueueRecord::Take(QueueTakeRecord {
                receipt,
                state,
                revision: safe_integer(wire.revision)?,
            }))
        }
    }
}

fn receipt_identity(
    request_id: String,
    session_id: String,
    directory: String,
    request_hash: String,
    accepted_at: u64,
) -> Result<QueueReceiptIdentity, QueueManifestError> {
    if !ascii_id(&request_id, 8, 128)
        || !ascii_id(&session_id, 4, 128)
        || directory.is_empty()
        || js_len(&directory) > 16384
        || js_trim(&directory) != directory
    {
        return Err(QueueManifestError::InvalidResponse);
    }
    Ok(QueueReceiptIdentity {
        request_id,
        session_id: SessionId::new(session_id),
        directory,
        request_hash: parse_digest(&request_hash).ok_or(QueueManifestError::InvalidResponse)?,
        accepted_at: safe_integer(accepted_at)?,
    })
}

fn validate_receipt_owner(
    identity: &QueueReceiptIdentity,
    owner: &QueueRequestOwner,
) -> Result<(), QueueManifestError> {
    if identity.request_id != owner.request_id
        || identity.session_id != owner.scope.session
        || identity.directory != owner.scope.directory
        || identity.accepted_at > MAX_SAFE_INTEGER
    {
        return Err(QueueManifestError::ReceiptMismatch);
    }
    Ok(())
}

fn validate_enqueue_receipt(receipt: &QueueEnqueueReceipt) -> Result<(), QueueManifestError> {
    if !valid_item_id(receipt.item_id.as_str())
        || !message_id(receipt.message_id.as_str())
        || receipt.context_ids.len() > QUEUE_MANIFEST_ENTRIES
        || receipt
            .context_ids
            .iter()
            .any(|id| !message_id(id.as_str()) || id == &receipt.message_id)
        || receipt.context_ids.iter().collect::<HashSet<_>>().len() != receipt.context_ids.len()
    {
        return Err(QueueManifestError::InvalidInput);
    }
    Ok(())
}

fn prefix_input(
    wire: PrefixWire,
    origin: MetadataOrigin,
) -> Result<QueuePrefixInput, QueueManifestError> {
    if wire.format != "manifest-v1"
        || !message_id(&wire.id)
        || js_len(&wire.composer) > 200_000
        || !matches!(wire.delivery.as_deref(), None | Some("steer"))
    {
        return Err(QueueManifestError::InvalidResponse);
    }
    let input = QueuePrefixInput {
        id: MessageId::new(wire.id),
        item_ids: wire.item_ids.into_iter().map(QueuedItemId::new).collect(),
        composer: wire.composer,
        send_config: config(wire.send_config)?,
        steer: wire.delivery.is_some(),
        prompt_parts: wire
            .prompt_parts
            .map(|wire| parts(wire, origin))
            .transpose()?,
    };
    validate_item_ids(&input.item_ids).map_err(|_| QueueManifestError::InvalidResponse)?;
    Ok(input)
}

fn validate_context_ids(
    ids: &[MessageId],
    item: &QueueManifestItem,
) -> Result<(), QueueManifestError> {
    let legacy = legacy_count(&item.context);
    if ids.len() != legacy + item.prompt_parts.context.len()
        || item
            .prompt_parts
            .context
            .iter()
            .zip(ids.iter().skip(legacy))
            .any(|(part, id)| part.id.as_ref().is_some_and(|expected| expected != id))
    {
        return Err(QueueManifestError::ReceiptMismatch);
    }
    Ok(())
}

fn validate_item(item: &QueueManifestItem) -> Result<(), QueueManifestError> {
    validate_item_for(item, false)
}

fn validate_item_for(
    item: &QueueManifestItem,
    allow_empty: bool,
) -> Result<(), QueueManifestError> {
    if js_len(&item.content) > 200_000
        || item
            .context_preview
            .as_ref()
            .is_some_and(|v| v.0.len() > 103)
        || item.attachment_manifest.len()
            + parts_count(&item.prompt_parts)
            + legacy_count(&item.context)
            > QUEUE_MANIFEST_ENTRIES
        || (!allow_empty
            && js_trim(&item.text).is_empty()
            && item.attachment_manifest.is_empty()
            && item.prompt_parts.file_references.is_empty()
            && item.context.is_empty()
            && item.prompt_parts.context.is_empty())
    {
        return Err(QueueManifestError::InvalidInput);
    }
    validate_config(&item.send_config)?;
    validate_parts(&item.prompt_parts)?;
    for file in &item.attachment_manifest {
        validate_descriptor(&file.attachment)?;
        if file.agent_path.as_deref().is_some_and(str::is_empty) {
            return Err(QueueManifestError::InvalidInput);
        }
        if let Some(mention) = &file.mention {
            validate_mention(mention)?;
        }
    }
    for entry in &item.context {
        let text = match entry {
            QueueLegacyContext::Context {
                text,
                metadata,
                instructions,
            } => {
                validate_metadata(metadata)?;
                if instructions
                    .as_deref()
                    .is_some_and(|s| js_trim(s).is_empty())
                {
                    return Err(QueueManifestError::InvalidInput);
                }
                text
            }
            QueueLegacyContext::Instruction { text } | QueueLegacyContext::Synthetic { text } => {
                text
            }
        };
        if js_trim(text).is_empty() {
            return Err(QueueManifestError::InvalidInput);
        }
    }
    Ok(())
}

fn validate_parts(parts: &QueuePromptParts) -> Result<(), QueueManifestError> {
    if parts_count(parts) > QUEUE_MANIFEST_ENTRIES {
        return Err(QueueManifestError::InvalidInput);
    }
    for file in &parts.file_references {
        validate_file_reference(file)?;
    }
    for agent in &parts.agents {
        if !identifier(&agent.name) {
            return Err(QueueManifestError::InvalidInput);
        }
        if let Some(mention) = &agent.mention {
            validate_mention(mention)?;
        }
    }
    for skill in &parts.skills {
        if !identifier(&skill.id) {
            return Err(QueueManifestError::InvalidInput);
        }
        if let Some(mention) = &skill.mention {
            validate_mention(mention)?;
        }
    }
    if let Some(metadata) = &parts.metadata {
        validate_metadata(metadata)?;
    }
    let mut context_ids = HashSet::new();
    for context in &parts.context {
        if js_trim(&context.text).is_empty()
            || context
                .id
                .as_ref()
                .is_some_and(|id| !message_id(id.as_str()))
        {
            return Err(QueueManifestError::InvalidInput);
        }
        if let Some(id) = &context.id
            && !context_ids.insert(id)
        {
            return Err(QueueManifestError::InvalidInput);
        }
        if let Some(metadata) = &context.metadata {
            validate_metadata(metadata)?;
        }
    }
    Ok(())
}

fn validate_config(config: &QueuedSendConfig) -> Result<(), QueueManifestError> {
    if !identifier(&config.provider_id)
        || !identifier(&config.model_id)
        || config.agent.as_deref().is_some_and(|s| !identifier(s))
        || config.variant.as_deref().is_some_and(|s| !identifier(s))
    {
        return Err(QueueManifestError::InvalidInput);
    }
    Ok(())
}

fn validate_descriptor(file: &QueueAttachmentDescriptor) -> Result<(), QueueManifestError> {
    if !ascii_id(&file.request_id, 8, 128)
        || !ascii_id(&file.attachment_id, 1, 128)
        || !valid_filename(&file.filename)
        || !valid_mime(&file.mime_type)
        || file.size > QUEUE_BLOB_BYTES as u64
    {
        return Err(QueueManifestError::InvalidInput);
    }
    Ok(())
}

fn validate_mention(mention: &QueueMention) -> Result<(), QueueManifestError> {
    if mention.start > mention.end || mention.end > MAX_SAFE_INTEGER {
        return Err(QueueManifestError::InvalidInput);
    }
    Ok(())
}

fn validate_metadata(metadata: &Metadata) -> Result<(), QueueManifestError> {
    fn valid(value: &Value, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match value {
            Value::Array(values) => values.iter().all(|value| valid(value, depth + 1)),
            Value::Object(values) => values.values().all(|value| valid(value, depth + 1)),
            Value::Number(number) => captured_json_number(number).is_some(),
            _ => true,
        }
    }
    if !metadata.0.values().all(|value| valid(value, 1)) {
        return Err(QueueManifestError::InvalidInput);
    }
    Ok(())
}

// Captured Rust integers retain their exact value before a mutation. A finite
// f64 is already the Number value JavaScript parses, even outside the safe range.
// The backend's metadata schema is z.json(), with no integer .safe() restriction.
fn captured_json_number(number: &serde_json::Number) -> Option<f64> {
    if number.is_f64() {
        return number.as_f64().filter(|value| value.is_finite());
    }
    if let Some(value) = number.as_i64() {
        if value.unsigned_abs() > MAX_SAFE_INTEGER {
            return None;
        }
        return Some(value as f64);
    }
    number
        .as_u64()
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .map(|value| value as f64)
}

fn metadata_from_wire(
    values: Map<String, Value>,
    origin: MetadataOrigin,
) -> Result<Metadata, QueueManifestError> {
    fn project(value: &mut Value) -> Result<(), QueueManifestError> {
        match value {
            Value::Number(number) => {
                // Response JSON was produced by JavaScript. Decimal integer
                // spellings can describe rounded f64 values beyond MAX_SAFE.
                // Project that known Number semantics only on this read path.
                let value = number.as_f64().ok_or(QueueManifestError::InvalidResponse)?;
                *number = serde_json::Number::from_f64(value)
                    .ok_or(QueueManifestError::InvalidResponse)?;
            }
            Value::Array(values) => {
                for value in values {
                    project(value)?;
                }
            }
            Value::Object(values) => {
                for value in values.values_mut() {
                    project(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let mut metadata = Metadata(values);
    if matches!(origin, MetadataOrigin::JavaScript) {
        for value in metadata.0.values_mut() {
            project(value)?;
        }
    }
    Ok(metadata)
}

fn optional_metadata_equal(left: Option<&Metadata>, right: Option<&Metadata>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => metadata_equal(left, right),
        (None, None) => true,
        _ => false,
    }
}

fn metadata_equal(left: &Metadata, right: &Metadata) -> bool {
    json_objects_equal(&left.0, &right.0)
}

fn json_objects_equal(left: &Map<String, Value>, right: &Map<String, Value>) -> bool {
    left.len() == right.len()
        && left.iter().all(|(key, value)| {
            right
                .get(key)
                .is_some_and(|other| json_values_equal(value, other))
        })
}

fn json_values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            left == right
                || captured_json_number(left)
                    .zip(captured_json_number(right))
                    .is_some_and(|(left, right)| left == right)
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_values_equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => json_objects_equal(left, right),
        _ => left == right,
    }
}

fn validate_item_ids(ids: &[QueuedItemId]) -> Result<(), QueueManifestError> {
    if ids.is_empty()
        || ids.len() > 20
        || ids.iter().any(|id| !valid_item_id(id.as_str()))
        || ids.iter().collect::<HashSet<_>>().len() != ids.len()
    {
        return Err(QueueManifestError::InvalidInput);
    }
    Ok(())
}

fn parse_message_ids(ids: Vec<String>) -> Result<Vec<MessageId>, QueueManifestError> {
    if ids.len() > QUEUE_MANIFEST_ENTRIES
        || ids.iter().any(|id| !message_id(id))
        || ids.iter().collect::<HashSet<_>>().len() != ids.len()
    {
        return Err(QueueManifestError::InvalidResponse);
    }
    Ok(ids.into_iter().map(MessageId::new).collect())
}

fn legacy_count(context: &[QueueLegacyContext]) -> usize {
    context
        .iter()
        .map(|part| match part {
            QueueLegacyContext::Context {
                instructions: Some(_),
                ..
            } => 2,
            _ => 1,
        })
        .sum()
}
fn context_count(item: &QueueManifestItem) -> usize {
    legacy_count(&item.context) + item.prompt_parts.context.len()
}
fn parts_count(parts: &QueuePromptParts) -> usize {
    parts.file_references.len() + parts.agents.len() + parts.skills.len() + parts.context.len()
}

pub(crate) fn queue_file_uri(value: &str) -> bool {
    if value.len() > 65536
        || !value.starts_with("file:///")
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return false;
    }
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && (bytes
                .get(index + 1)
                .is_none_or(|byte| !byte.is_ascii_hexdigit())
                || bytes
                    .get(index + 2)
                    .is_none_or(|byte| !byte.is_ascii_hexdigit()))
        {
            return false;
        }
    }
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "file"
            && url.host_str().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() != "/"
    })
}

fn validate_file_reference(file: &QueueFileReference) -> Result<(), QueueManifestError> {
    if !queue_file_uri(&file.uri)
        || file.name.is_empty()
        || file.name.len() > 4096
        || file.name.chars().any(char::is_control)
    {
        return Err(QueueManifestError::InvalidInput);
    }
    if let Some(mention) = &file.mention {
        validate_mention(mention)?;
    }
    Ok(())
}

fn file_references(
    wire: Vec<FileReferenceWire>,
) -> Result<Vec<QueueFileReference>, QueueManifestError> {
    if wire.len() > QUEUE_MANIFEST_ENTRIES {
        return Err(QueueManifestError::InvalidInput);
    }
    wire.into_iter()
        .map(|file| {
            let file = QueueFileReference {
                uri: file.uri,
                name: file.name,
                description: file.description,
                mention: file.mention.map(mention).transpose()?,
            };
            validate_file_reference(&file)?;
            Ok(file)
        })
        .collect()
}
fn total_entries(items: &[QueueTakenItem]) -> usize {
    items
        .iter()
        .map(|item| {
            item.item.attachment_manifest.len()
                + legacy_count(&item.item.context)
                + parts_count(&item.item.prompt_parts)
        })
        .sum()
}

fn config_value(config: &QueuedSendConfig) -> Value {
    let mut value = json!({"providerID": config.provider_id, "modelID": config.model_id});
    if let Some(agent) = &config.agent {
        value["agent"] = json!(agent);
    }
    if let Some(variant) = &config.variant {
        value["variant"] = json!(variant);
    }
    value
}
fn mention_value(mention: &QueueMention) -> Value {
    json!({"start": mention.start, "end": mention.end, "text": mention.text})
}
fn parts_value(parts: &QueuePromptParts) -> Value {
    let agents: Vec<_> = parts
        .agents
        .iter()
        .map(|agent| {
            let mut value = json!({"name": agent.name});
            if let Some(mention) = &agent.mention {
                value["mention"] = mention_value(mention);
            }
            value
        })
        .collect();
    let skills: Vec<_> = parts
        .skills
        .iter()
        .map(|skill| {
            let mut value = json!({"id": skill.id});
            if let Some(mention) = &skill.mention {
                value["mention"] = mention_value(mention);
            }
            value
        })
        .collect();
    let context: Vec<_> = parts
        .context
        .iter()
        .map(|context| {
            let mut value = json!({"text": context.text});
            if let Some(id) = &context.id {
                value["id"] = json!(id.as_str());
            }
            if let Some(description) = &context.description {
                value["description"] = json!(description);
            }
            if let Some(metadata) = &context.metadata {
                value["metadata"] = json!(metadata);
            }
            value
        })
        .collect();
    let mut value = json!({"agents": agents, "skills": skills, "context": context});
    if !parts.file_references.is_empty() {
        value["fileReferences"] = json!(parts.file_references);
    }
    if let Some(metadata) = &parts.metadata {
        value["metadata"] = json!(metadata);
    }
    value
}
fn item_value(item: &QueueManifestItem) -> Value {
    let attachments: Vec<_> = item
        .attachment_manifest
        .iter()
        .map(|file| {
            let d = &file.attachment;
            let mut value = json!({"blobId": hex_digest(&d.blob_id), "requestId": d.request_id,
            "attachmentId": d.attachment_id, "filename": d.filename, "mimeType": d.mime_type,
            "size": d.size, "sha256": hex_digest(&d.sha256)});
            if let Some(path) = &file.agent_path {
                value["agentPath"] = json!(path);
            }
            if let Some(description) = &file.description {
                value["description"] = json!(description);
            }
            if let Some(mention) = &file.mention {
                value["mention"] = mention_value(mention);
            }
            value
        })
        .collect();
    let context: Vec<_> = item.context.iter().map(legacy_context_value).collect();
    let value = json!({"format":"manifest-v1", "content": item.content, "text":item.text,
        "attachmentManifest":attachments, "context":context, "sendConfig":config_value(&item.send_config),
        "promptParts":parts_value(&item.prompt_parts)});
    value
}

fn legacy_context_value(context: &QueueLegacyContext) -> Value {
    match context {
        QueueLegacyContext::Context {
            text,
            metadata,
            instructions,
        } => {
            let mut value = json!({"kind":"context", "text":text, "metadata":metadata});
            if let Some(instructions) = instructions {
                value["instructions"] = json!(instructions);
            }
            value
        }
        QueueLegacyContext::Instruction { text } => json!({"kind":"instruction", "text":text}),
        QueueLegacyContext::Synthetic { text } => json!({"kind":"synthetic", "text":text}),
    }
}

fn capture_bound(
    kind: &str,
    owner: &QueueRequestOwner,
    input: &Value,
) -> Result<(), QueueManifestError> {
    // The backend bounds its canonical hash envelope, not just the POST body.
    let envelope = json!({"kind":kind, "sessionId":owner.scope.session.as_str(),
        "directory":owner.scope.directory, "input":input});
    if !valid_json_depth(&envelope, 0) {
        return Err(QueueManifestError::InvalidInput);
    }
    encode(&envelope).map(|_| ())
}

fn capture_manifest_bound(
    owner: &QueueRequestOwner,
    input: &Value,
    preview: Option<&PayloadPreview>,
) -> Result<(), QueueManifestError> {
    let envelope = json!({"kind":"enqueue", "sessionId":owner.scope.session.as_str(),
        "directory":owner.scope.directory, "input":input});
    if !valid_json_depth(&envelope, 0) {
        return Err(QueueManifestError::InvalidInput);
    }
    let encoded = encode(&envelope)?;
    let mut extra = Vec::new();
    if let Some(preview) = preview {
        extra.extend_from_slice(b",\"contextPreview\":");
        preview.append_json(&mut extra)?;
    }
    if extra.len() > QUEUE_MANIFEST_BYTES - encoded.len() {
        return Err(QueueManifestError::MetadataTooLarge);
    }
    Ok(())
}
fn valid_json_depth(value: &Value, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    match value {
        Value::Array(values) => values
            .iter()
            .all(|value| valid_json_depth(value, depth + 1)),
        Value::Object(values) => values
            .values()
            .all(|value| valid_json_depth(value, depth + 1)),
        _ => true,
    }
}
fn encode(value: &Value) -> Result<Vec<u8>, QueueManifestError> {
    let bytes = serde_json::to_vec(value).map_err(|_| QueueManifestError::InvalidInput)?;
    if bytes.len() > QUEUE_MANIFEST_BYTES {
        return Err(QueueManifestError::MetadataTooLarge);
    }
    Ok(bytes)
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, QueueManifestError> {
    if bytes.len() > QUEUE_MANIFEST_BYTES {
        return Err(QueueManifestError::MetadataTooLarge);
    }
    validate_json_text(bytes)?;
    serde_json::from_slice(bytes).map_err(|_| QueueManifestError::InvalidResponse)
}

fn validate_json_text(bytes: &[u8]) -> Result<(), QueueManifestError> {
    // The preview byte-string visitor accepts escaped WTF-8 surrogates. Literal
    // malformed UTF-8 and unescaped string controls must still be refused.
    std::str::from_utf8(bytes).map_err(|_| QueueManifestError::InvalidResponse)?;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in bytes {
        if !in_string {
            in_string = byte == b'"';
        } else if escaped {
            escaped = false;
        } else {
            match byte {
                b'"' => in_string = false,
                b'\\' => escaped = true,
                0..=31 => return Err(QueueManifestError::InvalidResponse),
                _ => {}
            }
        }
    }
    Ok(())
}
fn decode_input<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, QueueManifestError> {
    decode(bytes).map_err(|error| match error {
        QueueManifestError::InvalidResponse => QueueManifestError::InvalidInput,
        _ => error,
    })
}
async fn post_record(
    owner: &QueueRequestOwner,
    operation: &str,
    body: &[u8],
) -> Result<QueueRecord, QueueManifestError> {
    let request = owner
        .scope
        .authorized(Method::POST, owner.url(&[operation], false))
        .header(CONTENT_TYPE.as_str(), "application/json")
        .body(body.to_vec());
    parse_record(&json_body(send(request).await?).await?, owner)
}
async fn send(request: RequestBuilder) -> Result<Response, QueueManifestError> {
    request.send().await.map_err(request_error)
}
fn request_error(error: reqwest::Error) -> QueueManifestError {
    if error.is_timeout() {
        QueueManifestError::TimedOut
    } else {
        QueueManifestError::RequestFailed
    }
}
async fn require_ok(response: &mut Response) -> Result<(), QueueManifestError> {
    let status = response.status();
    if status == StatusCode::OK {
        return Ok(());
    }
    if status == StatusCode::UNAUTHORIZED {
        return Err(QueueManifestError::AuthenticationRequired);
    }
    if status.is_redirection() {
        return Err(QueueManifestError::Redirected);
    }
    let fallback = QueueManifestError::Http {
        status: status.as_u16(),
    };
    if !content_type(response.headers(), "application/json") {
        return Err(fallback);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if chunk.len() > ERROR_BYTES - bytes.len() {
            return Err(fallback);
        }
        bytes.extend_from_slice(&chunk);
    }
    #[derive(Deserialize)]
    struct ErrorWire {
        code: Option<String>,
    }
    let code = serde_json::from_slice::<ErrorWire>(&bytes)
        .ok()
        .and_then(|wire| wire.code);
    Err(match (status.as_u16(), code.as_deref()) {
        (404, Some("queue_request_unknown")) => QueueManifestError::UnknownRequest,
        (409, Some("queue_request_conflict")) => QueueManifestError::Conflict,
        (409, Some("queue_prefix_stale")) => QueueManifestError::PrefixStale,
        (409 | 503, Some("queue_request_pending")) => QueueManifestError::Pending,
        (503, Some("queue_request_recovery_required")) => QueueManifestError::RecoveryRequired,
        (413, Some("queue_request_too_large")) => QueueManifestError::MetadataTooLarge,
        _ => fallback,
    })
}
async fn json_body(mut response: Response) -> Result<Vec<u8>, QueueManifestError> {
    require_ok(&mut response).await?;
    if !content_type(response.headers(), "application/json") {
        return Err(QueueManifestError::InvalidResponse);
    }
    if response
        .content_length()
        .is_some_and(|length| length > QUEUE_MANIFEST_BYTES as u64)
    {
        return Err(QueueManifestError::MetadataTooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if chunk.len() > QUEUE_MANIFEST_BYTES - bytes.len() {
            return Err(QueueManifestError::MetadataTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
fn content_type(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}
fn safe_integer(value: u64) -> Result<u64, QueueManifestError> {
    if value <= MAX_SAFE_INTEGER {
        Ok(value)
    } else {
        Err(QueueManifestError::InvalidResponse)
    }
}
fn js_len(value: &str) -> usize {
    value.encode_utf16().count()
}
fn js_trim(value: &str) -> &str {
    // ECMAScript whitespace differs from Rust's for FEFF and NEXT LINE.
    value.trim_matches(|c: char| c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}'))
}
fn ascii_id(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
fn message_id(value: &str) -> bool {
    value
        .strip_prefix("msg_")
        .is_some_and(|suffix| ascii_id(suffix, 1, 120))
}
fn valid_item_id(value: &str) -> bool {
    !value.is_empty() && js_len(value) <= 256
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && js_len(value) <= 4096
        && !value
            .chars()
            .any(|c| matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}'))
}
fn valid_filename(value: &str) -> bool {
    identifier(value) && !matches!(value, "." | "..") && !value.contains(['/', '\\'])
}
fn valid_mime(value: &str) -> bool {
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b))
    };
    value.len() <= 255
        && value
            .split_once('/')
            .is_some_and(|(a, b)| token(a) && token(b))
}
fn parse_digest(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut digest = [0; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = nibble(pair[0])? * 16 + nibble(pair[1])?;
    }
    Some(digest)
}
fn hex_digest(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(64);
    for byte in digest {
        value.push(HEX[(byte >> 4) as usize] as char);
        value.push(HEX[(byte & 15) as usize] as char);
    }
    value
}

#[cfg(test)]
#[path = "queue_manifest_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "queue_payload_tests.rs"]
mod payload_tests;
