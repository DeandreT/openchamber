//! Private, versioned local drafts and unresolved prompt submissions.
//!
//! The caller supplies a path inside the existing native configuration directory.
//! Hold one journal for the application's lifetime. Run open/save on a serialized
//! background worker, and hydrate before accepting edits. A send must wait for a
//! durable snapshot containing its Sending attempt before contacting the server.
//! This module neither starts requests nor chooses a profile directory.
//!
//! The lock is a separate stable inode and must never be removed while in use.
//! Writes replace the whole snapshot, so callers must enqueue snapshots in mutation
//! order. A failed open grants no write handle: malformed stores are left intact.
//!
//! Version 8 adds compact queue operation owners and separately held spool chunks.
//! Version 7 retains full draft content, explicit prepared attachment ownership,
//! preparation holds, exact staging targets, pending starter captures and
//! retired-blob deletion intents.
//! Versions 1 through 6 migrate without accessing any attachment content.
//! Version 6 adds the queue reservation intent saved before acquiring it.
//! Version 5 adds whether each send supersedes open requests; older
//! versions load with it set, as every send of theirs did. Version 4 stores
//! whole prompts: text, attachment references, agent and skill
//! references, metadata, the ordered synthetic context items with their IDs,
//! the prompt ID and delivery. Versions 1 to 3 held text prompts and load as
//! prompts without parts. Frozen prompts contain bounded attachment references;
//! v7 drafts separately retain immutable prepared-byte descriptors in the catalog.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use openchamber_native_core::composer_selection::{SelectionOwner, SelectionSnapshot};
use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::ids::{
    AttachmentId, DraftId, MessageId, PartId, RuntimeId, SessionId,
};
use openchamber_native_core::permission_mode::PermissionMode;
use openchamber_native_core::send_config::{ModelChoice, Selection};
use openchamber_native_core::transcript::{
    Message, MessageInfo, Part, PartContent, PartTime, Role,
};
use openchamber_native_transport::prompt::{Delivery, Prompt};
use openchamber_native_transport::prompt_parts::{
    AgentMention, Mention, Metadata, PromptFile, PromptParts, SkillRef, SyntheticContext,
};
use serde::{Deserialize, Deserializer, Serialize};

use crate::attachment_catalog::{
    self, AttachmentCatalog, PreparationHold, StoredAttachment, StoredPreparation,
};
use crate::attachment_spool::BlobId;
use crate::create_attempt::{CreateAttempt, CreationPhase};
use crate::draft_storage::{self, ContentStorage, LegacyDraft, StoredDraft, required_option};
use crate::queue_storage::QueueStorage;
use crate::send_attempt::{Admission, SendAttempt, SendOrigin, SessionKey};
use crate::starter_capture::StarterCapture;
use openchamber_native_transport::session_create::CreateSession;

const VERSION: u32 = 8;
const MAX_FILE: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_TEXT: usize = 1024 * 1024;
pub(crate) const MAX_ENTRIES: usize = 4096;
pub(crate) const MAX_ID: usize = 4096;
pub(crate) const MAX_DIRECTORY: usize = 16 * 1024;
/// An attachment URI, including a small inline `data:` URI.
pub(crate) const MAX_URI: usize = 64 * 1024;
pub(crate) const MAX_METADATA: usize = 64 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Contains no tasks, endpoints, authorization headers or credentials.
#[derive(Default)]
#[cfg_attr(test, derive(Clone))]
pub(crate) struct JournalSnapshot {
    pub drafts: Vec<(DraftKey, Draft)>,
    pub attempts: Vec<(SessionKey, SendAttempt)>,
    pub creations: Vec<(SessionKey, CreateAttempt)>,
    pub selections: Vec<(SelectionOwner, SelectionSnapshot)>,
    pub attachments: AttachmentCatalog,
    pub preparations: Vec<PreparationHold>,
    pub retired_blobs: Vec<BlobId>,
    pub starter_captures: Vec<StarterCapture>,
    pub queue: QueueStorage,
}

/// Prospective snapshots borrow retained payloads. Only ownership metadata and a
/// small attachment-only draft need to be constructed for an admission check.
pub(crate) struct JournalView<'a> {
    pub drafts: Vec<(&'a DraftKey, &'a Draft)>,
    pub attempts: Vec<(&'a SessionKey, &'a SendAttempt)>,
    pub creations: Vec<(&'a SessionKey, &'a CreateAttempt)>,
    pub selections: &'a [(SelectionOwner, SelectionSnapshot)],
    pub attachments: &'a AttachmentCatalog,
    pub preparations: &'a [PreparationHold],
    pub retired_blobs: &'a [BlobId],
    pub starter_captures: Vec<&'a StarterCapture>,
    pub added_files: Option<(&'a DraftKey, &'a Draft)>,
    pub queue: &'a QueueStorage,
}

impl<'a> From<&'a JournalSnapshot> for JournalView<'a> {
    fn from(snapshot: &'a JournalSnapshot) -> Self {
        Self {
            drafts: snapshot
                .drafts
                .iter()
                .map(|(owner, draft)| (owner, draft))
                .collect(),
            attempts: snapshot
                .attempts
                .iter()
                .map(|(key, attempt)| (key, attempt))
                .collect(),
            creations: snapshot
                .creations
                .iter()
                .map(|(key, creation)| (key, creation))
                .collect(),
            selections: &snapshot.selections,
            attachments: &snapshot.attachments,
            preparations: &snapshot.preparations,
            retired_blobs: &snapshot.retired_blobs,
            starter_captures: snapshot.starter_captures.iter().collect(),
            added_files: None,
            queue: &snapshot.queue,
        }
    }
}

impl JournalView<'_> {
    /// Uses the same semantic validator, storage choices and bounded JSON writer as save.
    pub(crate) fn validate(&self) -> Result<(), JournalError> {
        let store = Store::capture_view(self, |text| text, |reservation| reservation)?;
        encode_store(&store, io::sink())?;
        Ok(())
    }

    fn files_for(&self, owner: &DraftKey) -> &[openchamber_native_core::draft_content::DraftFile] {
        self.added_files
            .filter(|(target, _)| *target == owner)
            .map_or(&[], |(_, additions)| additions.content.files.as_slice())
    }
}

pub(crate) enum LoadedJournal {
    Missing,
    Present(Box<JournalSnapshot>),
}

/// Error messages contain schema reasons only, never persisted field values.
#[derive(Debug)]
pub(crate) enum JournalError {
    Io(io::Error),
    Locked,
    Invalid(&'static str),
    UnsupportedVersion(u32),
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "journal I/O failure: {}", error.kind()),
            Self::Locked => f.write_str("journal is already locked"),
            Self::Invalid(reason) => write!(f, "invalid journal: {reason}"),
            Self::UnsupportedVersion(version) => write!(f, "unsupported journal version {version}"),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<io::Error> for JournalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Rename has committed in both cases. An unsynced directory is an explicit
/// durability uncertainty; the caller must keep sending disabled and surface it.
/// Returning it separately avoids claiming a failed write preserved the old file
/// after the filesystem has already replaced it.
#[derive(Debug)]
pub(crate) enum JournalCommit {
    Durable,
    DirectorySyncUncertain(io::Error),
}

pub(crate) struct MutationJournal {
    path: PathBuf,
    parent: File,
    _lock: File,
}

impl MutationJournal {
    /// Opens and validates the complete store under an exclusive nonblocking lock.
    /// Missing differs from a successfully persisted empty snapshot. Failure
    /// returns no writer and never repairs, truncates or rewrites the target.
    pub(crate) fn open(path: &Path) -> Result<(Self, LoadedJournal), JournalError> {
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(JournalError::Invalid(
                "journal path must be an absolute file path",
            ));
        }
        let parent_path = path
            .parent()
            .ok_or(JournalError::Invalid("missing parent"))?;
        private_directory(parent_path)?;
        let parent = File::open(parent_path)?;
        let lock_path = sibling(path, ".lock");
        let lock = match private_options().create_new(true).open(&lock_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                check_private_file(&lock_path)?;
                OpenOptions::new().read(true).write(true).open(&lock_path)?
            }
            Err(error) => return Err(error.into()),
        };
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(JournalError::Locked),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let journal = Self {
            path: path.to_owned(),
            parent,
            _lock: lock,
        };
        let loaded = journal.load()?;
        Ok((journal, loaded))
    }

    fn load(&self) -> Result<LoadedJournal, JournalError> {
        match fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(LoadedJournal::Missing);
            }
            Err(error) => return Err(error.into()),
            Ok(_) => check_private_file(&self.path)?,
        }
        let file = File::open(&self.path)?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE {
            return Err(JournalError::Invalid("file size limit exceeded"));
        }
        let store = decode_store(&bytes)?;
        Ok(LoadedJournal::Present(Box::new(store.into_snapshot()?)))
    }

    /// Atomically replaces the complete store. Every Err occurs before rename,
    /// preserving the previous target. No failed snapshot is silently dropped.
    /// Existing unrelated writers must obey the same stable lock protocol.
    pub(crate) fn save(
        &mut self,
        snapshot: &JournalSnapshot,
    ) -> Result<JournalCommit, JournalError> {
        let store = Store::capture(snapshot)?;
        let bytes = encode_store(&store, Vec::new())?;
        let (temporary, mut file) = create_temporary(&self.path)?;
        let result = (|| {
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            Ok::<(), io::Error>(())
        })();
        if let Err(error) = result {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        Ok(match self.parent.sync_all() {
            Ok(()) => JournalCommit::Durable,
            Err(error) => JournalCommit::DirectorySyncUncertain(error),
        })
    }
}

/// Bounds JSON escaping expansion before allocating the encoded snapshot.
struct BoundedBytes<W = Vec<u8>>(W, usize);

impl<W: Write> Write for BoundedBytes<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.1.saturating_add(bytes.len()) as u64 > MAX_FILE {
            return Err(io::Error::other("journal size limit exceeded"));
        }
        let written = self.0.write(bytes)?;
        self.1 += written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_store<W: Write>(store: &impl Serialize, writer: W) -> Result<W, JournalError> {
    let mut bytes = BoundedBytes(writer, 0);
    serde_json::to_writer(&mut bytes, store)
        .map_err(|_| JournalError::Invalid("encoded file size limit exceeded"))?;
    Ok(bytes.0)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .expect("validated journal filename")
        .to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn create_temporary(path: &Path) -> io::Result<(PathBuf, File)> {
    // A crash can leave a temp file behind, and process IDs can be reused.
    // Skip collisions without reading, truncating or deleting that file.
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = sibling(path, &format!(".{}.{}.tmp", std::process::id(), sequence));
        match private_options().create_new(true).open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "journal temporary file collisions",
    ))
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn private_directory(path: &Path) -> Result<(), JournalError> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {
            // Persist the newly-created directory's name as well as its contents.
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(JournalError::Invalid("parent must be a real directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn check_private_file(path: &Path) -> Result<(), JournalError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(JournalError::Invalid("expected a regular private file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.nlink() != 1 {
            return Err(JournalError::Invalid(
                "file permissions or link count are unsafe",
            ));
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Store<
    D = StoredDraft,
    P = StoredPrompt,
    R = crate::send_attempt::QueueReservation,
    S = String,
    Q = QueueStorage,
> {
    version: u32,
    drafts: Vec<D>,
    attempts: Vec<StoredAttempt<D, P, R>>,
    creations: Vec<StoredCreation<D, P>>,
    selections: Vec<crate::selection_storage::StoredSelection>,
    attachments: Vec<StoredAttachment>,
    preparations: Vec<StoredPreparation>,
    retired_blobs: Vec<String>,
    starter_captures: Vec<StoredStarterCapture<D, P, S>>,
    queue: Q,
}

/// A store captured from a journal view, borrowing its queue storage.
type CapturedStore<'view, S, R> = Store<StoredDraft<S>, StoredPrompt<S>, R, S, &'view QueueStorage>;

/// Kept separate so a v7 file cannot smuggle v8 ownership into its old schema.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionSevenStore {
    version: u32,
    drafts: Vec<StoredDraft>,
    attempts: Vec<StoredAttempt>,
    creations: Vec<StoredCreation>,
    selections: Vec<crate::selection_storage::StoredSelection>,
    attachments: Vec<StoredAttachment>,
    preparations: Vec<StoredPreparation>,
    retired_blobs: Vec<String>,
    starter_captures: Vec<StoredStarterCapture>,
}

/// Version 3: version 4's layout with text-only prompts.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextStore {
    version: u32,
    drafts: Vec<LegacyDraft>,
    attempts: Vec<LegacyAttempt<TextOnlyPrompt>>,
    creations: Vec<LegacyCreation<TextOnlyPrompt>>,
    selections: Vec<crate::selection_storage::StoredSelection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousStore {
    version: u32,
    drafts: Vec<LegacyDraft>,
    attempts: Vec<LegacyAttempt<TextOnlyPrompt>>,
    creations: Vec<LegacyCreation<TextOnlyPrompt>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyStore {
    version: u32,
    drafts: Vec<LegacyDraft>,
    attempts: Vec<LegacyAttempt<TextOnlyPrompt>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyFullStore {
    version: u32,
    drafts: Vec<LegacyDraft>,
    attempts: Vec<LegacyAttempt>,
    creations: Vec<LegacyCreation>,
    selections: Vec<crate::selection_storage::StoredSelection>,
}

fn decode_store(bytes: &[u8]) -> Result<Store, JournalError> {
    if bytes.len() as u64 > MAX_FILE {
        return Err(JournalError::Invalid("file size limit exceeded"));
    }
    // Version parsing remains separate so unknown future formats grant no writer.
    #[derive(Deserialize)]
    struct Version {
        version: u32,
    }
    let version: Version = serde_json::from_slice(bytes)
        .map_err(|_| JournalError::Invalid("malformed JSON or version"))?;
    if !matches!(version.version, 1..=VERSION) {
        return Err(JournalError::UnsupportedVersion(version.version));
    }
    let malformed = |_| JournalError::Invalid("malformed journal schema");
    let (drafts, attempts, creations, selections) = match version.version {
        1 => {
            let store: LegacyStore = serde_json::from_slice(bytes).map_err(malformed)?;
            (
                store.drafts,
                store
                    .attempts
                    .into_iter()
                    .map(|attempt| attempt.upgrade_text(store.version))
                    .collect::<Result<_, _>>()?,
                Vec::new(),
                Vec::new(),
            )
        }
        2 => {
            let store: PreviousStore = serde_json::from_slice(bytes).map_err(malformed)?;
            (
                store.drafts,
                store
                    .attempts
                    .into_iter()
                    .map(|attempt| attempt.upgrade_text(store.version))
                    .collect::<Result<_, _>>()?,
                store
                    .creations
                    .into_iter()
                    .map(LegacyCreation::upgrade_text)
                    .collect(),
                Vec::new(),
            )
        }
        3 => {
            let store: TextStore = serde_json::from_slice(bytes).map_err(malformed)?;
            (
                store.drafts,
                store
                    .attempts
                    .into_iter()
                    .map(|attempt| attempt.upgrade_text(store.version))
                    .collect::<Result<_, _>>()?,
                store
                    .creations
                    .into_iter()
                    .map(LegacyCreation::upgrade_text)
                    .collect(),
                store.selections,
            )
        }
        4..=6 => {
            let store: LegacyFullStore = serde_json::from_slice(bytes).map_err(malformed)?;
            let attempts = store
                .attempts
                .into_iter()
                .map(|attempt| attempt.upgrade(store.version))
                .collect::<Result<_, _>>()?;
            (
                store.drafts,
                attempts,
                store
                    .creations
                    .into_iter()
                    .map(LegacyCreation::upgrade)
                    .collect(),
                store.selections,
            )
        }
        7 => {
            let store: VersionSevenStore = serde_json::from_slice(bytes).map_err(malformed)?;
            if store.version != 7 {
                return Err(JournalError::Invalid("invalid v7 journal version"));
            }
            return Ok(Store {
                version: VERSION,
                drafts: store.drafts,
                attempts: store.attempts,
                creations: store.creations,
                selections: store.selections,
                attachments: store.attachments,
                preparations: store.preparations,
                retired_blobs: store.retired_blobs,
                starter_captures: store.starter_captures,
                queue: QueueStorage::default(),
            });
        }
        _ => return serde_json::from_slice(bytes).map_err(malformed),
    };
    Ok(Store {
        version: VERSION,
        drafts: drafts
            .into_iter()
            .map(|draft| draft.upgrade(None))
            .collect(),
        attempts,
        creations,
        selections,
        attachments: Vec::new(),
        preparations: Vec::new(),
        retired_blobs: Vec::new(),
        starter_captures: Vec::new(),
        queue: QueueStorage::default(),
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredStarterCapture<D = StoredDraft, P = StoredPrompt, S = String> {
    draft: D,
    project: String,
    preset_text: S,
    /// The capture identity, source text, exact selection and converted parts.
    prompt: P,
    #[serde(deserialize_with = "required_option")]
    permission_mode: Option<StoredPermissionMode>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredPermissionMode {
    Ask,
    Safety,
    Auto,
}

impl StoredStarterCapture {
    fn capture_with<'a, S>(
        capture: &'a StarterCapture,
        text: impl Fn(&'a str) -> S + Copy,
    ) -> StoredStarterCapture<StoredDraft<S>, StoredPrompt<S>, S> {
        StoredStarterCapture {
            draft: StoredDraft::capture_with(&capture.owner, &capture.draft, &[], text),
            project: capture.project.clone(),
            preset_text: text(&capture.preset_text),
            prompt: StoredPrompt::capture_parts(
                &capture.id,
                &capture.draft.text,
                &capture.parts,
                capture.selection.model.as_ref(),
                capture.selection.agent_name.as_deref(),
                None,
                text,
            ),
            permission_mode: capture.permission_mode.map(|mode| match mode {
                PermissionMode::Ask => StoredPermissionMode::Ask,
                PermissionMode::Safety => StoredPermissionMode::Safety,
                PermissionMode::Auto => StoredPermissionMode::Auto,
            }),
        }
    }

    fn restore(self) -> Result<StarterCapture, JournalError> {
        let (owner, draft) = self.draft.into_draft()?;
        let prompt = self.prompt.into_prompt();
        if prompt.text != draft.text || prompt.delivery.is_some() {
            return Err(JournalError::Invalid(
                "starter prompt differs from its source capture",
            ));
        }
        Ok(StarterCapture {
            id: prompt.message_id,
            owner,
            project: self.project,
            draft,
            preset_text: self.preset_text,
            selection: Selection {
                model: prompt.model,
                agent_name: prompt.agent,
            },
            parts: prompt.parts,
            permission_mode: self.permission_mode.map(|mode| match mode {
                StoredPermissionMode::Ask => PermissionMode::Ask,
                StoredPermissionMode::Safety => PermissionMode::Safety,
                StoredPermissionMode::Auto => PermissionMode::Auto,
            }),
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCreation<D = StoredDraft, P = StoredPrompt> {
    session: String,
    project: String,
    draft: D,
    prompt: P,
    resources: Vec<String>,
    #[serde(deserialize_with = "required_option")]
    request_model: Option<StoredModel>,
    #[serde(deserialize_with = "required_option")]
    request_agent: Option<String>,
    created: i64,
    phase: StoredCreationPhase,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredCreationPhase {
    Uncertain,
    Confirmed,
    Mismatch,
    Deleted,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Owner {
    runtime: String,
    directory: String,
    target: Target,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Session(String),
    NewSession(String),
}

#[derive(Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    bound(deserialize = "D: Deserialize<'de>, P: Deserialize<'de>, R: Deserialize<'de>")
)]
struct StoredAttempt<D = StoredDraft, P = StoredPrompt, R = crate::send_attempt::QueueReservation> {
    origin: StoredOrigin,
    runtime: String,
    session: String,
    draft: D,
    prompt: P,
    resources: Vec<String>,
    optimistic_part_id: String,
    created: i64,
    admission: StoredAdmission,
    #[serde(deserialize_with = "required_option")]
    supersedes: Option<bool>,
    queue_owned: bool,
    #[serde(deserialize_with = "required_option")]
    reservation: Option<R>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAttempt<P = StoredPrompt> {
    #[serde(default)]
    origin: StoredOrigin,
    runtime: String,
    session: String,
    draft: LegacyDraft,
    prompt: P,
    optimistic_part_id: String,
    created: i64,
    admission: StoredAdmission,
    #[serde(default, deserialize_with = "supplied_option")]
    supersedes: Option<Option<bool>>,
    #[serde(default, deserialize_with = "supplied_option")]
    queue_owned: Option<Option<bool>>,
    #[serde(default, deserialize_with = "supplied_option")]
    reservation: Option<Option<crate::send_attempt::QueueReservation>>,
}

// Legacy version gates must distinguish an absent field from an explicit null.
fn supplied_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCreation<P = StoredPrompt> {
    session: String,
    project: String,
    draft: LegacyDraft,
    prompt: P,
    request_model: Option<StoredModel>,
    request_agent: Option<String>,
    created: i64,
    phase: StoredCreationPhase,
}

impl LegacyAttempt<TextOnlyPrompt> {
    fn upgrade_text(self, version: u32) -> Result<StoredAttempt, JournalError> {
        LegacyAttempt {
            origin: self.origin,
            runtime: self.runtime,
            session: self.session,
            draft: self.draft,
            prompt: self.prompt.upgrade(),
            optimistic_part_id: self.optimistic_part_id,
            created: self.created,
            admission: self.admission,
            supersedes: self.supersedes,
            queue_owned: self.queue_owned,
            reservation: self.reservation,
        }
        .upgrade(version)
    }
}

impl LegacyCreation<TextOnlyPrompt> {
    fn upgrade_text(self) -> StoredCreation {
        LegacyCreation {
            session: self.session,
            project: self.project,
            draft: self.draft,
            prompt: self.prompt.upgrade(),
            request_model: self.request_model,
            request_agent: self.request_agent,
            created: self.created,
            phase: self.phase,
        }
        .upgrade()
    }
}

impl LegacyAttempt {
    fn upgrade(self, version: u32) -> Result<StoredAttempt, JournalError> {
        if version < 5 && (self.queue_owned.is_some() || self.supersedes.is_some()) {
            return Err(JournalError::Invalid("queue policy before version 5"));
        }
        if version < 6 && self.reservation.is_some() {
            return Err(JournalError::Invalid("reservation intent before version 6"));
        }
        let (supersedes, queue_owned) = if version >= 5 {
            (
                self.supersedes
                    .flatten()
                    .ok_or(JournalError::Invalid("attempt without supersession"))?,
                self.queue_owned
                    .flatten()
                    .ok_or(JournalError::Invalid("attempt without queue ownership"))?,
            )
        } else {
            (true, false)
        };
        let prompt = self.prompt.into_prompt();
        Ok(StoredAttempt {
            origin: self.origin,
            runtime: self.runtime,
            session: self.session,
            draft: self.draft.upgrade(Some(&prompt)),
            prompt: StoredPrompt::capture(&prompt),
            resources: Vec::new(),
            optimistic_part_id: self.optimistic_part_id,
            created: self.created,
            admission: self.admission,
            supersedes: Some(supersedes),
            queue_owned,
            reservation: self.reservation.flatten(),
        })
    }
}

impl LegacyCreation {
    fn upgrade(self) -> StoredCreation {
        let prompt = self.prompt.into_prompt();
        StoredCreation {
            session: self.session,
            project: self.project,
            draft: self.draft.upgrade(Some(&prompt)),
            prompt: StoredPrompt::capture(&prompt),
            resources: Vec::new(),
            request_model: self.request_model,
            request_agent: self.request_agent,
            created: self.created,
            phase: self.phase,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredOrigin {
    #[default]
    Existing,
    Created,
    Deleted,
}

/// A prompt of versions 1 to 3.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextOnlyPrompt {
    message_id: String,
    text: String,
    model: Option<StoredModel>,
    agent: Option<String>,
    delivery: Option<StoredDelivery>,
}

impl TextOnlyPrompt {
    fn upgrade(self) -> StoredPrompt {
        StoredPrompt {
            message_id: self.message_id,
            text: self.text,
            files: Vec::new(),
            agents: Vec::new(),
            skills: Vec::new(),
            metadata: None,
            context: Vec::new(),
            model: self.model,
            agent: self.agent,
            delivery: self.delivery,
        }
    }
}

/// Every field is required, so a version 4 file never loses a part silently.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPrompt<S = String> {
    message_id: String,
    text: S,
    files: Vec<StoredFile<S>>,
    agents: Vec<StoredAgent>,
    skills: Vec<String>,
    #[serde(deserialize_with = "required_option")]
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
    /// Admitted in this order before the prompt.
    context: Vec<StoredContext<S>>,
    #[serde(deserialize_with = "required_option")]
    model: Option<StoredModel>,
    #[serde(deserialize_with = "required_option")]
    agent: Option<String>,
    #[serde(deserialize_with = "required_option")]
    delivery: Option<StoredDelivery>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFile<S> {
    uri: S,
    #[serde(deserialize_with = "required_option")]
    name: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAgent {
    name: String,
    #[serde(deserialize_with = "required_option")]
    mention: Option<StoredMention>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredMention {
    start: u32,
    end: u32,
    text: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredContext<S> {
    id: String,
    text: S,
    #[serde(deserialize_with = "required_option")]
    description: Option<S>,
    #[serde(deserialize_with = "required_option")]
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredModel {
    provider: String,
    model: String,
    #[serde(deserialize_with = "required_option")]
    variant: Option<String>,
}

impl StoredModel {
    pub(crate) fn capture(model: &ModelChoice) -> Self {
        Self {
            provider: model.provider_id.clone(),
            model: model.model_id.clone(),
            variant: model.variant.clone(),
        }
    }

    pub(crate) fn into_choice(self) -> ModelChoice {
        ModelChoice {
            provider_id: self.provider,
            model_id: self.model,
            variant: self.variant,
        }
    }
}

impl StoredPrompt {
    fn capture(prompt: &Prompt) -> Self {
        Self::capture_with(prompt, str::to_owned)
    }

    fn capture_with<'a, S>(
        prompt: &'a Prompt,
        text: impl Fn(&'a str) -> S + Copy,
    ) -> StoredPrompt<S> {
        Self::capture_parts(
            &prompt.message_id,
            &prompt.text,
            &prompt.parts,
            prompt.model.as_ref(),
            prompt.agent.as_deref(),
            prompt.delivery,
            text,
        )
    }

    fn capture_parts<'a, S>(
        message_id: &MessageId,
        source_text: &'a str,
        parts: &'a PromptParts,
        model: Option<&ModelChoice>,
        agent: Option<&str>,
        delivery: Option<Delivery>,
        text: impl Fn(&'a str) -> S + Copy,
    ) -> StoredPrompt<S> {
        StoredPrompt {
            message_id: message_id.to_string(),
            text: text(source_text),
            files: parts
                .files
                .iter()
                .map(|file| StoredFile {
                    uri: text(&file.uri),
                    name: file.name.clone(),
                })
                .collect(),
            agents: parts
                .agents
                .iter()
                .map(|agent| StoredAgent {
                    name: agent.name.clone(),
                    mention: agent.mention.as_ref().map(|mention| StoredMention {
                        start: mention.start,
                        end: mention.end,
                        text: mention.text.clone(),
                    }),
                })
                .collect(),
            skills: parts.skills.iter().map(|skill| skill.id.clone()).collect(),
            metadata: parts.metadata.as_ref().map(|metadata| metadata.0.clone()),
            context: parts
                .context
                .iter()
                .map(|item| StoredContext {
                    id: item.id.to_string(),
                    text: text(&item.text),
                    description: item.description.as_deref().map(text),
                    metadata: item.metadata.as_ref().map(|metadata| metadata.0.clone()),
                })
                .collect(),
            model: model.map(StoredModel::capture),
            agent: agent.map(str::to_owned),
            delivery: delivery.map(|delivery| match delivery {
                Delivery::Steer => StoredDelivery::Steer,
                Delivery::Queue => StoredDelivery::Queue,
            }),
        }
    }

    fn into_prompt(self) -> Prompt {
        Prompt {
            message_id: MessageId::from(self.message_id),
            text: self.text,
            parts: PromptParts {
                files: self
                    .files
                    .into_iter()
                    .map(|file| PromptFile {
                        uri: file.uri,
                        name: file.name,
                    })
                    .collect(),
                agents: self
                    .agents
                    .into_iter()
                    .map(|agent| AgentMention {
                        name: agent.name,
                        mention: agent.mention.map(|mention| Mention {
                            start: mention.start,
                            end: mention.end,
                            text: mention.text,
                        }),
                    })
                    .collect(),
                skills: self.skills.into_iter().map(|id| SkillRef { id }).collect(),
                metadata: self.metadata.map(Metadata),
                context: self
                    .context
                    .into_iter()
                    .map(|item| SyntheticContext {
                        id: MessageId::from(item.id),
                        text: item.text,
                        description: item.description,
                        metadata: item.metadata.map(Metadata),
                    })
                    .collect(),
            },
            model: self.model.map(StoredModel::into_choice),
            agent: self.agent,
            delivery: self.delivery.map(|delivery| match delivery {
                StoredDelivery::Steer => Delivery::Steer,
                StoredDelivery::Queue => Delivery::Queue,
            }),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredDelivery {
    Steer,
    Queue,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredAdmission {
    Sending,
    Admitted,
    Uncertain,
}

pub(crate) fn identifier(value: &str) -> Result<(), JournalError> {
    if value.trim().is_empty() || value.len() > MAX_ID || value.chars().any(char::is_control) {
        return Err(JournalError::Invalid("invalid identifier"));
    }
    Ok(())
}

impl Owner {
    pub(crate) fn capture(key: &DraftKey) -> Self {
        Self {
            runtime: key.runtime().to_string(),
            directory: key.directory().to_owned(),
            target: match key.target() {
                DraftTarget::Session(id) => Target::Session(id.to_string()),
                DraftTarget::NewSession(id) => Target::NewSession(id.to_string()),
            },
        }
    }

    pub(crate) fn into_key(self) -> Result<DraftKey, JournalError> {
        identifier(&self.runtime)?;
        if self.directory.len() > MAX_DIRECTORY || self.directory.chars().any(char::is_control) {
            return Err(JournalError::Invalid("invalid directory"));
        }
        let target = match self.target {
            Target::Session(id) => {
                identifier(&id)?;
                DraftTarget::Session(SessionId::from(id))
            }
            Target::NewSession(id) => {
                identifier(&id)?;
                DraftTarget::NewSession(DraftId::from(id))
            }
        };
        let key = DraftKey::new(RuntimeId::from(self.runtime), &self.directory, target)
            .ok_or(JournalError::Invalid("invalid directory"))?;
        if key.directory() != self.directory {
            return Err(JournalError::Invalid("directory is not canonical"));
        }
        Ok(key)
    }
}

impl Store {
    fn capture(
        snapshot: &JournalSnapshot,
    ) -> Result<
        Store<
            StoredDraft,
            StoredPrompt,
            crate::send_attempt::QueueReservation,
            String,
            &QueueStorage,
        >,
        JournalError,
    > {
        Self::capture_view(&JournalView::from(snapshot), str::to_owned, Clone::clone)
    }

    fn capture_view<'a, 'view, S, R>(
        snapshot: &'a JournalView<'view>,
        text: impl Fn(&'a str) -> S + Copy,
        reservation: impl Fn(&'a crate::send_attempt::QueueReservation) -> R,
    ) -> Result<CapturedStore<'view, S, R>, JournalError> {
        let attempt_drafts: Vec<_> = snapshot
            .attempts
            .iter()
            .map(|(_, attempt)| {
                StoredDraft::capture_attempt_with(
                    &attempt.owner,
                    &attempt.draft,
                    &attempt.prompt,
                    text,
                )
            })
            .collect();
        let creation_drafts: Vec<_> = snapshot
            .creations
            .iter()
            .map(|(_, attempt)| {
                StoredDraft::capture_attempt_with(
                    &attempt.source,
                    &attempt.draft,
                    &attempt.prompt,
                    text,
                )
            })
            .collect();
        let attempt_storage: Vec<_> = attempt_drafts.iter().map(StoredDraft::storage).collect();
        let creation_storage: Vec<_> = creation_drafts.iter().map(StoredDraft::storage).collect();
        validate_snapshot(snapshot, &attempt_storage, &creation_storage)?;
        let attempts = snapshot
            .attempts
            .iter()
            .zip(attempt_drafts)
            .map(|((key, attempt), draft)| StoredAttempt {
                origin: match attempt.origin {
                    SendOrigin::Existing => StoredOrigin::Existing,
                    SendOrigin::Created => StoredOrigin::Created,
                    SendOrigin::Deleted => StoredOrigin::Deleted,
                },
                runtime: key.runtime.to_string(),
                session: key.session.to_string(),
                draft,
                prompt: StoredPrompt::capture_with(&attempt.prompt, text),
                resources: attempt.resources.iter().map(ToString::to_string).collect(),
                optimistic_part_id: attempt.optimistic.parts[0].id.to_string(),
                created: attempt.optimistic.info.created,
                admission: match attempt.admission {
                    Admission::Preparing | Admission::Sending { .. } => StoredAdmission::Sending,
                    Admission::Admitted => StoredAdmission::Admitted,
                    Admission::Uncertain => StoredAdmission::Uncertain,
                },
                supersedes: Some(attempt.supersedes),
                queue_owned: attempt.queue_owned,
                reservation: attempt.reservation.as_ref().map(&reservation),
            })
            .collect();
        Ok(Store {
            version: VERSION,
            queue: snapshot.queue,
            starter_captures: snapshot
                .starter_captures
                .iter()
                .map(|capture| StoredStarterCapture::capture_with(capture, text))
                .collect(),
            attachments: attachment_catalog::capture(snapshot.attachments),
            preparations: snapshot
                .preparations
                .iter()
                .map(StoredPreparation::capture)
                .collect(),
            retired_blobs: snapshot
                .retired_blobs
                .iter()
                .map(|id| id.as_str().to_owned())
                .collect(),
            selections: snapshot
                .selections
                .iter()
                .map(|(owner, state)| {
                    crate::selection_storage::StoredSelection::capture(owner, state)
                })
                .collect(),
            drafts: snapshot
                .drafts
                .iter()
                .map(|(owner, draft)| {
                    StoredDraft::capture_with(owner, draft, snapshot.files_for(owner), text)
                })
                .collect(),
            attempts,
            creations: snapshot
                .creations
                .iter()
                .zip(creation_drafts)
                .map(|((key, attempt), draft)| StoredCreation {
                    session: key.session.to_string(),
                    project: attempt.project.clone(),
                    draft,
                    prompt: StoredPrompt::capture_with(&attempt.prompt, text),
                    resources: attempt.resources.iter().map(ToString::to_string).collect(),
                    request_model: attempt.request.model().map(StoredModel::capture),
                    request_agent: attempt.request.agent().map(str::to_owned),
                    created: attempt.created,
                    phase: match attempt.phase {
                        CreationPhase::Preparing
                        | CreationPhase::Creating
                        | CreationPhase::Uncertain => StoredCreationPhase::Uncertain,
                        CreationPhase::Confirming | CreationPhase::Confirmed => {
                            StoredCreationPhase::Confirmed
                        }
                        CreationPhase::Mismatch => StoredCreationPhase::Mismatch,
                        CreationPhase::Deleted => StoredCreationPhase::Deleted,
                    },
                })
                .collect(),
        })
    }

    fn into_snapshot(self) -> Result<JournalSnapshot, JournalError> {
        if self.version != VERSION {
            return Err(JournalError::UnsupportedVersion(self.version));
        }
        if self.drafts.len() > MAX_ENTRIES
            || self.attempts.len() > MAX_ENTRIES
            || self.creations.len() > MAX_ENTRIES
            || self.starter_captures.len() > MAX_ENTRIES
        {
            return Err(JournalError::Invalid("entry count limit exceeded"));
        }
        let attempt_storage: Vec<_> = self
            .attempts
            .iter()
            .map(|attempt| attempt.draft.storage())
            .collect();
        let creation_storage: Vec<_> = self
            .creations
            .iter()
            .map(|attempt| attempt.draft.storage())
            .collect();
        let drafts = self
            .drafts
            .into_iter()
            .map(StoredDraft::into_draft)
            .collect::<Result<_, _>>()?;
        let mut attempts = Vec::with_capacity(self.attempts.len());
        for stored in self.attempts {
            let supersedes = stored
                .supersedes
                .ok_or(JournalError::Invalid("attempt without supersession"))?;
            let prompt = stored.prompt.into_prompt();
            let (owner, draft) = stored.draft.into_capture(&prompt)?;
            let optimistic = Message {
                info: MessageInfo {
                    id: prompt.message_id.clone(),
                    role: Role::User,
                    created: stored.created,
                    completed: None,
                    notice: None,
                    assistant: None,
                    compaction: None,
                    synthetic: None,
                },
                parts: vec![Part::text(
                    PartId::from(stored.optimistic_part_id),
                    prompt.text.clone(),
                )],
            };
            attempts.push((
                SessionKey {
                    runtime: RuntimeId::from(stored.runtime),
                    session: SessionId::from(stored.session),
                },
                SendAttempt {
                    origin: match stored.origin {
                        StoredOrigin::Existing => SendOrigin::Existing,
                        StoredOrigin::Created => SendOrigin::Created,
                        StoredOrigin::Deleted => SendOrigin::Deleted,
                    },
                    owner,
                    draft,
                    prompt,
                    resources: stored
                        .resources
                        .into_iter()
                        .map(AttachmentId::from)
                        .collect(),
                    optimistic,
                    admission: match stored.admission {
                        StoredAdmission::Sending | StoredAdmission::Uncertain => {
                            Admission::Uncertain
                        }
                        StoredAdmission::Admitted => Admission::Admitted,
                    },
                    supersedes,
                    queue_owned: stored.queue_owned,
                    reservation: stored.reservation,
                },
            ));
        }
        let mut creations = Vec::with_capacity(self.creations.len());
        for stored in self.creations {
            let prompt = stored.prompt.into_prompt();
            let (source, draft) = stored.draft.into_capture(&prompt)?;
            let key = SessionKey {
                runtime: source.runtime().clone(),
                session: SessionId::from(stored.session),
            };
            let request = CreateSession::new(
                key.session.clone(),
                source.directory(),
                stored.request_model.map(StoredModel::into_choice),
                stored.request_agent,
            )
            .map_err(|_| JournalError::Invalid("invalid captured creation request"))?;
            creations.push((
                key,
                CreateAttempt {
                    source,
                    draft,
                    request,
                    prompt,
                    resources: stored
                        .resources
                        .into_iter()
                        .map(AttachmentId::from)
                        .collect(),
                    created: stored.created,
                    project: stored.project,
                    navigation: None,
                    phase: match stored.phase {
                        StoredCreationPhase::Uncertain => CreationPhase::Uncertain,
                        StoredCreationPhase::Confirmed => CreationPhase::Confirmed,
                        StoredCreationPhase::Mismatch => CreationPhase::Mismatch,
                        StoredCreationPhase::Deleted => CreationPhase::Deleted,
                    },
                },
            ));
        }
        let snapshot = JournalSnapshot {
            queue: self.queue,
            drafts,
            attempts,
            creations,
            starter_captures: self
                .starter_captures
                .into_iter()
                .map(StoredStarterCapture::restore)
                .collect::<Result<_, _>>()?,
            attachments: attachment_catalog::restore(self.attachments)?,
            preparations: self
                .preparations
                .into_iter()
                .map(StoredPreparation::restore)
                .collect::<Result<_, _>>()?,
            retired_blobs: self
                .retired_blobs
                .into_iter()
                .map(|id| attachment_catalog::blob_id(&id))
                .collect::<Result<_, _>>()?,
            selections: self
                .selections
                .into_iter()
                .map(crate::selection_storage::StoredSelection::restore)
                .collect::<Result<_, _>>()?,
        };
        validate_snapshot(
            &JournalView::from(&snapshot),
            &attempt_storage,
            &creation_storage,
        )?;
        Ok(snapshot)
    }
}

fn validate_snapshot(
    snapshot: &JournalView<'_>,
    attempt_storage: &[ContentStorage],
    creation_storage: &[ContentStorage],
) -> Result<(), JournalError> {
    if attempt_storage.len() != snapshot.attempts.len()
        || creation_storage.len() != snapshot.creations.len()
    {
        return Err(JournalError::Invalid(
            "capture storage layout differs from snapshot",
        ));
    }
    crate::selection_storage::validate(snapshot.selections)?;
    for (owner, state) in snapshot.selections {
        if let Some(source) = &state.staged_from {
            let valid = snapshot.creations.iter().any(|(key, creation)| {
                creation.source == source.draft
                    && owner.draft.runtime() == &key.runtime
                    && owner.draft.target() == &DraftTarget::Session(key.session.clone())
                    && match &owner.project {
                        Some(project) => project.as_str() == creation.project,
                        // A Chat belongs to no registered project.
                        None => {
                            creation.project == openchamber_native_core::chat_draft::CHAT_PROJECT_ID
                        }
                    }
            });
            if !valid {
                return Err(JournalError::Invalid("staged selection lacks its creation"));
            }
        }
    }
    if snapshot.drafts.len() > MAX_ENTRIES
        || snapshot.attempts.len() > MAX_ENTRIES
        || snapshot.creations.len() > MAX_ENTRIES
        || snapshot.starter_captures.len() > MAX_ENTRIES
    {
        return Err(JournalError::Invalid("entry count limit exceeded"));
    }
    let mut owners = HashSet::new();
    let mut size = 0;
    for (owner, selection) in snapshot.selections {
        let mut bytes = BoundedBytes(Vec::new(), 0);
        serde_json::to_writer(
            &mut bytes,
            &crate::selection_storage::StoredSelection::capture(owner, selection),
        )
        .map_err(|_| JournalError::Invalid("selection size limit exceeded"))?;
        charge(&mut size, bytes.0.len())?;
    }
    attachment_catalog::validate(
        &mut size,
        snapshot.attachments,
        snapshot.preparations,
        snapshot.retired_blobs,
    )?;
    let mut immutable_files = HashMap::new();
    for &(owner, draft) in &snapshot.drafts {
        charge_owner(&mut size, owner)?;
        Owner::capture(owner).into_key()?;
        draft_storage::validate_with_files(&mut size, draft, snapshot.files_for(owner))?;
        attachment_catalog::validate_draft(
            snapshot.attachments,
            owner,
            draft,
            &mut immutable_files,
        )?;
        if !owners.insert(owner) {
            return Err(JournalError::Invalid("duplicate draft owner"));
        }
    }
    if let Some((owner, additions)) = snapshot.added_files {
        if !owners.contains(&owner) {
            return Err(JournalError::Invalid(
                "attachment publication lacks draft owner",
            ));
        }
        attachment_catalog::validate_draft(
            snapshot.attachments,
            owner,
            additions,
            &mut immutable_files,
        )?;
    }
    // A session holds several acknowledged attempts but at most one that is
    // not yet acknowledged, and never both a creation and an attempt.
    let mut sessions = std::collections::HashMap::new();
    let mut messages = HashSet::new();
    let mut parts = HashSet::new();
    for ((key, attempt), storage) in snapshot.attempts.iter().copied().zip(attempt_storage) {
        charge_owner(&mut size, &attempt.owner)?;
        charge(
            &mut size,
            key.runtime.as_str().len() + key.session.as_str().len(),
        )?;
        identifier(key.runtime.as_str())?;
        identifier(key.session.as_str())?;
        Owner::capture(&attempt.owner).into_key()?;
        draft_storage::validate_capture(&mut size, &attempt.draft, *storage)?;
        attachment_catalog::validate_draft(
            snapshot.attachments,
            &attempt.owner,
            &attempt.draft,
            &mut immutable_files,
        )?;
        attachment_catalog::validate_resources(
            &mut size,
            snapshot.attachments,
            &attempt.owner,
            &attempt.resources,
        )?;
        if let Some(intent) = &attempt.reservation {
            if !attempt.queue_owned
                || intent.items.is_empty()
                || intent.items.len() > 20
                || intent.composer.len() > MAX_TEXT
            {
                return Err(JournalError::Invalid("invalid reservation intent"));
            }
            charge(&mut size, intent.composer.len())?;
            let mut ids = HashSet::new();
            for id in &intent.items {
                identifier(id)?;
                charge(&mut size, id.len())?;
                if !ids.insert(id) {
                    return Err(JournalError::Invalid("duplicate reservation item"));
                }
            }
        }
        validate_prompt(&mut size, &key.runtime, &attempt.prompt, &mut messages)?;
        let unacknowledged = sessions.entry(key).or_insert(0usize);
        if attempt.admission != Admission::Admitted {
            *unacknowledged += 1;
        }
        if attempt.owner.runtime() != &key.runtime
            || attempt.owner.target() != &DraftTarget::Session(key.session.clone())
            || *unacknowledged > 1
        {
            return Err(JournalError::Invalid(
                "invalid or duplicate attempt owner or identity",
            ));
        }
        if attempt.origin != SendOrigin::Existing {
            CreateSession::new(
                key.session.clone(),
                attempt.owner.directory(),
                attempt.prompt.model.clone(),
                attempt.prompt.agent.clone(),
            )
            .map_err(|_| JournalError::Invalid("invalid created-session send owner"))?;
        }
        let [part] = attempt.optimistic.parts.as_slice() else {
            return Err(JournalError::Invalid("expected one optimistic text part"));
        };
        identifier(part.id.as_str())?;
        charge(&mut size, part.id.as_str().len())?;
        if !parts.insert((&key.runtime, &part.id)) {
            return Err(JournalError::Invalid("duplicate optimistic part identity"));
        }
        let expected_info = MessageInfo {
            id: attempt.prompt.message_id.clone(),
            role: Role::User,
            created: attempt.optimistic.info.created,
            completed: None,
            notice: None,
            assistant: None,
            compaction: None,
            synthetic: None,
        };
        if attempt.optimistic.info != expected_info
            || part.time != PartTime::default()
            || !matches!(&part.content, PartContent::Text { text } if text == &attempt.prompt.text)
        {
            return Err(JournalError::Invalid(
                "optimistic message differs from captured prompt",
            ));
        }
        if attempt.optimistic.info.created < 0 {
            return Err(JournalError::Invalid("negative optimistic creation time"));
        }
    }
    let mut sources = HashSet::new();
    let mut created = HashSet::new();
    for ((key, attempt), storage) in snapshot.creations.iter().copied().zip(creation_storage) {
        charge_owner(&mut size, &attempt.source)?;
        charge(&mut size, attempt.project.len())?;
        charge(
            &mut size,
            key.runtime.as_str().len() + key.session.as_str().len(),
        )?;
        identifier(key.runtime.as_str())?;
        identifier(key.session.as_str())?;
        Owner::capture(&attempt.source).into_key()?;
        draft_storage::validate_capture(&mut size, &attempt.draft, *storage)?;
        attachment_catalog::validate_draft(
            snapshot.attachments,
            &attempt.source,
            &attempt.draft,
            &mut immutable_files,
        )?;
        attachment_catalog::validate_resources(
            &mut size,
            snapshot.attachments,
            &attempt.source,
            &attempt.resources,
        )?;
        identifier(&attempt.project)?;
        validate_prompt(&mut size, &key.runtime, &attempt.prompt, &mut messages)?;
        if attempt.source.runtime() != &key.runtime
            || !matches!(attempt.source.target(), DraftTarget::NewSession(_))
            || attempt.request.id() != &key.session
            || attempt.request.directory() != attempt.source.directory()
            || sessions.contains_key(key)
            || !created.insert(key)
            || (attempt.phase != CreationPhase::Deleted && !sources.insert(&attempt.source))
            || attempt.created < 0
            || attempt.prompt.delivery.is_some()
            || attempt.prompt.model.is_none()
        {
            return Err(JournalError::Invalid(
                "invalid or duplicate creation identity",
            ));
        }
        let expected = CreateSession::new(
            key.session.clone(),
            attempt.source.directory(),
            attempt.prompt.model.clone(),
            attempt.prompt.agent.clone(),
        )
        .map_err(|_| JournalError::Invalid("invalid captured creation configuration"))?;
        if expected != attempt.request {
            return Err(JournalError::Invalid(
                "creation request differs from captured configuration",
            ));
        }
    }
    let mut starter_owners = HashSet::new();
    let mut starter_ids = HashSet::new();
    for capture in &snapshot.starter_captures {
        charge_owner(&mut size, &capture.owner)?;
        Owner::capture(&capture.owner).into_key()?;
        identifier(&capture.project)?;
        if !matches!(capture.owner.target(), DraftTarget::NewSession(_))
            || attachment_catalog::remote_path(capture.owner.directory()).as_deref()
                != Some(capture.owner.directory())
            || !starter_owners.insert(&capture.owner)
            || !starter_ids.insert(&capture.id)
            || sources.contains(&capture.owner)
            || capture.selection.model.is_none()
            || capture.preset_text.trim().is_empty()
            || capture.preset_text.len() > MAX_TEXT
        {
            return Err(JournalError::Invalid(
                "invalid or duplicate starter capture owner or configuration",
            ));
        }
        charge(&mut size, capture.project.len() + capture.preset_text.len())?;
        draft_storage::validate(&mut size, &capture.draft)?;
        attachment_catalog::validate_draft(
            snapshot.attachments,
            &capture.owner,
            &capture.draft,
            &mut immutable_files,
        )?;
        attachment_catalog::validate_captured_parts(
            snapshot.attachments,
            &capture.owner,
            &capture.draft,
            &capture.parts,
        )?;
        reserve_message(
            &mut size,
            capture.owner.runtime(),
            &capture.id,
            &mut messages,
        )?;
        // StoredPrompt explicitly retains the source text again with its parts.
        charge(&mut size, capture.draft.text.len())?;
        validate_configuration(
            &mut size,
            capture.selection.model.as_ref(),
            capture.selection.agent_name.as_deref(),
        )?;
        validate_parts(
            &mut size,
            capture.owner.runtime(),
            &capture.draft.text,
            &capture.parts,
            &mut messages,
        )?;
        if capture
            .parts
            .agents
            .iter()
            .filter_map(|agent| agent.mention.as_ref())
            .any(|mention| !draft_storage::mention_matches(&capture.draft.text, mention))
        {
            return Err(JournalError::Invalid(
                "starter mention differs from its source text",
            ));
        }
    }
    for hold in snapshot.preparations {
        if immutable_files.get(&hold.id).is_some_and(|file| {
            matches!(
                &file.content,
                openchamber_native_core::draft_content::DraftFileContent::Reference { .. }
            )
        }) {
            return Err(JournalError::Invalid(
                "reference identity aliases a preparation hold",
            ));
        }
    }
    let mut queue_messages = messages
        .iter()
        .map(|(runtime, message)| (runtime.as_str(), message.as_str()))
        .collect();
    snapshot.queue.validate(
        snapshot.attachments,
        snapshot.preparations,
        snapshot.retired_blobs,
        &mut queue_messages,
    )?;
    for source in snapshot.queue.inline_sources() {
        charge_owner(&mut size, &source.owner)?;
        draft_storage::validate(&mut size, &source.draft)?;
        attachment_catalog::validate_draft(
            snapshot.attachments,
            &source.owner,
            &source.draft,
            &mut immutable_files,
        )?;
    }
    for file in immutable_files.values() {
        for id in std::iter::once(&file.id).chain(file.source_document_id.iter()) {
            if BlobId::parse(id.as_str()).is_ok_and(|blob| snapshot.queue.contains_blob(&blob)) {
                return Err(JournalError::Invalid(
                    "queue blob aliases a draft content root",
                ));
            }
        }
    }
    for retired in snapshot.retired_blobs {
        if immutable_files.values().any(|file| {
            file.id.as_str() == retired.as_str()
                || file
                    .source_document_id
                    .as_ref()
                    .is_some_and(|source| source.as_str() == retired.as_str())
        }) {
            return Err(JournalError::Invalid(
                "retired blob remains a draft content root",
            ));
        }
    }
    Ok(())
}

/// Bounds and identity checks of one stored prompt. Every prompt and context
/// ID is unique within its runtime across attempts, creations and starters.
fn validate_prompt<'a>(
    size: &mut usize,
    runtime: &'a RuntimeId,
    prompt: &'a Prompt,
    messages: &mut HashSet<(&'a RuntimeId, &'a MessageId)>,
) -> Result<(), JournalError> {
    if !prompt.is_well_formed() {
        return Err(JournalError::Invalid("malformed prompt"));
    }
    charge(size, prompt.text.len())?;
    if prompt.text.len() > MAX_TEXT {
        return Err(JournalError::Invalid("text size limit exceeded"));
    }
    reserve_message(size, runtime, &prompt.message_id, messages)?;
    validate_configuration(size, prompt.model.as_ref(), prompt.agent.as_deref())?;
    validate_parts(size, runtime, &prompt.text, &prompt.parts, messages)
}

fn reserve_message<'a>(
    size: &mut usize,
    runtime: &'a RuntimeId,
    id: &'a MessageId,
    messages: &mut HashSet<(&'a RuntimeId, &'a MessageId)>,
) -> Result<(), JournalError> {
    identifier(id.as_str())?;
    charge(size, id.as_str().len())?;
    if !id.as_str().starts_with("msg_") || !messages.insert((runtime, id)) {
        return Err(JournalError::Invalid(
            "invalid or duplicate admission identity",
        ));
    }
    Ok(())
}

fn validate_configuration(
    size: &mut usize,
    model: Option<&ModelChoice>,
    agent: Option<&str>,
) -> Result<(), JournalError> {
    if let Some(model) = model {
        charge(size, model.provider_id.len() + model.model_id.len())?;
        identifier(&model.provider_id)?;
        identifier(&model.model_id)?;
        if let Some(variant) = &model.variant {
            charge(size, variant.len())?;
            identifier(variant)?;
        }
    }
    if let Some(agent) = agent {
        charge(size, agent.len())?;
        identifier(agent)?;
    }
    Ok(())
}

fn validate_parts<'a>(
    size: &mut usize,
    runtime: &'a RuntimeId,
    text: &str,
    parts: &'a PromptParts,
    messages: &mut HashSet<(&'a RuntimeId, &'a MessageId)>,
) -> Result<(), JournalError> {
    if parts.files.len() + parts.agents.len() + parts.skills.len() + parts.context.len()
        > MAX_ENTRIES
    {
        return Err(JournalError::Invalid("entry count limit exceeded"));
    }
    for file in &parts.files {
        charge(size, file.uri.len())?;
        if file.uri.trim().is_empty()
            || file.uri.len() > MAX_URI
            || file.uri.chars().any(char::is_control)
        {
            return Err(JournalError::Invalid("attachment reference limit exceeded"));
        }
        if let Some(name) = &file.name {
            charge(size, name.len())?;
            identifier(name)?;
        }
    }
    draft_storage::validate_agents(size, &parts.agents, text)?;
    let mut skills = HashSet::new();
    for skill in &parts.skills {
        charge(size, skill.id.len())?;
        identifier(&skill.id)?;
        if !skills.insert(&skill.id) {
            return Err(JournalError::Invalid("duplicate skill identity"));
        }
    }
    validate_metadata(size, parts.metadata.as_ref())?;
    for item in &parts.context {
        reserve_message(size, runtime, &item.id, messages)?;
        charge(size, item.text.len())?;
        if item.text.trim().is_empty() || item.text.len() > MAX_TEXT {
            return Err(JournalError::Invalid("text size limit exceeded"));
        }
        if let Some(description) = &item.description {
            charge(size, description.len())?;
            if description.len() > MAX_TEXT {
                return Err(JournalError::Invalid("text size limit exceeded"));
            }
        }
        validate_metadata(size, item.metadata.as_ref())?;
    }
    Ok(())
}

fn validate_metadata(size: &mut usize, metadata: Option<&Metadata>) -> Result<(), JournalError> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    let mut bytes = BoundedBytes(Vec::new(), 0);
    serde_json::to_writer(&mut bytes, metadata)
        .map_err(|_| JournalError::Invalid("metadata size limit exceeded"))?;
    if bytes.0.len() > MAX_METADATA {
        return Err(JournalError::Invalid("metadata size limit exceeded"));
    }
    charge(size, bytes.0.len())
}

pub(crate) fn charge(size: &mut usize, additional: usize) -> Result<(), JournalError> {
    *size = size.saturating_add(additional);
    if *size as u64 > MAX_FILE {
        return Err(JournalError::Invalid(
            "aggregate snapshot size limit exceeded",
        ));
    }
    Ok(())
}

pub(crate) fn charge_owner(size: &mut usize, owner: &DraftKey) -> Result<(), JournalError> {
    let target = match owner.target() {
        DraftTarget::Session(id) => id.as_str(),
        DraftTarget::NewSession(id) => id.as_str(),
    };
    charge(
        size,
        owner.runtime().as_str().len() + owner.directory().len() + target.len(),
    )
}

#[cfg(test)]
#[path = "mutation_journal_tests.rs"]
mod tests;

#[cfg(test)]
mod borrowed_snapshot_tests {
    use super::*;

    #[test]
    fn canonical_preflight_borrows_pending_prompt_context_and_reservation() {
        let runtime = RuntimeId::from("runtime");
        let session = SessionId::from("session");
        let owner = DraftKey::new(
            runtime.clone(),
            "/project",
            DraftTarget::Session(session.clone()),
        )
        .expect("canonical captured owner");
        let prompt = Prompt {
            message_id: MessageId::from("msg_pending"),
            text: "p".repeat(MAX_TEXT),
            parts: PromptParts {
                context: vec![SyntheticContext {
                    id: MessageId::from("msg_context"),
                    text: "c".repeat(MAX_TEXT),
                    description: None,
                    metadata: None,
                }],
                ..PromptParts::default()
            },
            model: None,
            agent: None,
            delivery: None,
        };
        let optimistic = Message {
            info: MessageInfo {
                id: prompt.message_id.clone(),
                role: Role::User,
                created: 1,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
                synthetic: None,
            },
            parts: vec![Part::text(
                PartId::from("part_pending"),
                prompt.text.clone(),
            )],
        };
        let mut snapshot = JournalSnapshot::default();
        snapshot
            .drafts
            .push((owner.clone(), Draft::new("independent draft")));
        snapshot.attempts.push((
            SessionKey { runtime, session },
            SendAttempt {
                origin: SendOrigin::Existing,
                owner,
                draft: Draft::new("rollback"),
                prompt,
                resources: Vec::new(),
                optimistic,
                admission: Admission::Uncertain,
                supersedes: false,
                queue_owned: true,
                reservation: Some(crate::send_attempt::QueueReservation {
                    items: vec!["queued-item".to_owned()],
                    composer: "q".repeat(MAX_TEXT),
                }),
            },
        ));
        let view = JournalView::from(&snapshot);
        let borrowed = Store::capture_view(&view, |text| text, |reservation| reservation)
            .expect("complete pending snapshot preflight");
        let pending = &snapshot.attempts[0].1;
        assert_eq!(
            borrowed.attempts[0].prompt.text.as_ptr(),
            pending.prompt.text.as_ptr()
        );
        assert_eq!(
            borrowed.attempts[0].prompt.context[0].text.as_ptr(),
            pending.prompt.parts.context[0].text.as_ptr(),
        );
        assert!(std::ptr::eq(
            borrowed.attempts[0]
                .reservation
                .expect("borrowed reservation"),
            pending.reservation.as_ref().expect("original reservation"),
        ));
        assert_eq!(
            encode_store(&borrowed, Vec::new()).expect("borrowed bounded encoding"),
            encode_store(
                &Store::capture(&snapshot).expect("owned capture"),
                Vec::new()
            )
            .expect("owned bounded encoding"),
            "prospective checks and durable saves use the same complete v7 bytes",
        );
        view.validate()
            .expect("preflight includes the unresolved send without changing its identity");
        assert_eq!(snapshot.attempts[0].1.admission, Admission::Uncertain);
    }
}
