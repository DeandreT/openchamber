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
//! Version 4 stores whole prompts: text, attachment references, agent and skill
//! references, metadata, the ordered synthetic context items with their IDs,
//! the prompt ID and delivery. Versions 1 to 3 held text prompts and load as
//! prompts without parts. Attachments are references (a server path or URI);
//! content larger than `MAX_URI` must be uploaded before an attempt captures it.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use openchamber_native_core::composer_selection::{SelectionOwner, SelectionSnapshot};
use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::ids::{DraftId, MessageId, PartId, RuntimeId, SessionId};
use openchamber_native_core::send_config::ModelChoice;
use openchamber_native_core::transcript::{Message, MessageInfo, Part, Role};
use openchamber_native_transport::prompt::{Delivery, Prompt};
use openchamber_native_transport::prompt_parts::{
    AgentMention, Mention, Metadata, PromptFile, PromptParts, SkillRef, SyntheticContext,
};
use serde::{Deserialize, Serialize};

use crate::create_attempt::{CreateAttempt, CreationPhase};
use crate::send_attempt::{Admission, SendAttempt, SendOrigin, SessionKey};
use openchamber_native_transport::session_create::CreateSession;

const VERSION: u32 = 4;
const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_TEXT: usize = 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const MAX_ID: usize = 4096;
const MAX_DIRECTORY: usize = 16 * 1024;
/// An attachment URI, including a small inline `data:` URI.
const MAX_URI: usize = 64 * 1024;
const MAX_METADATA: usize = 64 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Contains no tasks, endpoints, authorization headers or credentials.
#[derive(Default)]
pub(crate) struct JournalSnapshot {
    pub drafts: Vec<(DraftKey, Draft)>,
    pub attempts: Vec<(SessionKey, SendAttempt)>,
    pub creations: Vec<(SessionKey, CreateAttempt)>,
    pub selections: Vec<(SelectionOwner, SelectionSnapshot)>,
}

pub(crate) enum LoadedJournal {
    Missing,
    Present(JournalSnapshot),
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
        // Read the version separately so a future format fails as unsupported,
        // even when its payload shape differs. Full parsing rejects duplicates.
        #[derive(Deserialize)]
        struct Version {
            version: u32,
        }
        let version: Version = serde_json::from_slice(&bytes)
            .map_err(|_| JournalError::Invalid("malformed JSON or version"))?;
        if !matches!(version.version, 1..=VERSION) {
            return Err(JournalError::UnsupportedVersion(version.version));
        }
        let store: Store = match version.version {
            1 => {
                let legacy: LegacyStore = serde_json::from_slice(&bytes)
                    .map_err(|_| JournalError::Invalid("malformed legacy schema"))?;
                Store {
                    version: legacy.version,
                    drafts: legacy.drafts,
                    attempts: legacy
                        .attempts
                        .into_iter()
                        .map(StoredAttempt::upgrade)
                        .collect(),
                    creations: Vec::new(),
                    selections: Vec::new(),
                }
            }
            2 => {
                let legacy: PreviousStore = serde_json::from_slice(&bytes)
                    .map_err(|_| JournalError::Invalid("malformed previous schema"))?;
                Store {
                    version: legacy.version,
                    drafts: legacy.drafts,
                    attempts: legacy
                        .attempts
                        .into_iter()
                        .map(StoredAttempt::upgrade)
                        .collect(),
                    creations: legacy
                        .creations
                        .into_iter()
                        .map(StoredCreation::upgrade)
                        .collect(),
                    selections: Vec::new(),
                }
            }
            3 => {
                let legacy: TextStore = serde_json::from_slice(&bytes)
                    .map_err(|_| JournalError::Invalid("malformed text-prompt schema"))?;
                Store {
                    version: legacy.version,
                    drafts: legacy.drafts,
                    attempts: legacy
                        .attempts
                        .into_iter()
                        .map(StoredAttempt::upgrade)
                        .collect(),
                    creations: legacy
                        .creations
                        .into_iter()
                        .map(StoredCreation::upgrade)
                        .collect(),
                    selections: legacy.selections,
                }
            }
            _ => serde_json::from_slice(&bytes)
                .map_err(|_| JournalError::Invalid("malformed schema"))?,
        };
        Ok(LoadedJournal::Present(store.into_snapshot()?))
    }

    /// Atomically replaces the complete store. Every Err occurs before rename,
    /// preserving the previous target. No failed snapshot is silently dropped.
    /// Existing unrelated writers must obey the same stable lock protocol.
    pub(crate) fn save(
        &mut self,
        snapshot: &JournalSnapshot,
    ) -> Result<JournalCommit, JournalError> {
        let store = Store::capture(snapshot)?;
        let mut bytes = BoundedBytes(Vec::new());
        serde_json::to_writer(&mut bytes, &store)
            .map_err(|_| JournalError::Invalid("encoded file size limit exceeded"))?;
        let (temporary, mut file) = create_temporary(&self.path)?;
        let result = (|| {
            file.write_all(&bytes.0)?;
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
struct BoundedBytes(Vec<u8>);

impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) as u64 > MAX_FILE {
            return Err(io::Error::other("journal size limit exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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
struct Store {
    version: u32,
    drafts: Vec<StoredDraft>,
    attempts: Vec<StoredAttempt>,
    creations: Vec<StoredCreation>,
    selections: Vec<crate::selection_storage::StoredSelection>,
}

/// Version 3: version 4's layout with text-only prompts.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextStore {
    version: u32,
    drafts: Vec<StoredDraft>,
    attempts: Vec<StoredAttempt<TextOnlyPrompt>>,
    creations: Vec<StoredCreation<TextOnlyPrompt>>,
    selections: Vec<crate::selection_storage::StoredSelection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousStore {
    version: u32,
    drafts: Vec<StoredDraft>,
    attempts: Vec<StoredAttempt<TextOnlyPrompt>>,
    creations: Vec<StoredCreation<TextOnlyPrompt>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyStore {
    version: u32,
    drafts: Vec<StoredDraft>,
    attempts: Vec<StoredAttempt<TextOnlyPrompt>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCreation<P = StoredPrompt> {
    session: String,
    project: String,
    draft: StoredDraft,
    prompt: P,
    request_model: Option<StoredModel>,
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Owner {
    runtime: String,
    directory: String,
    target: Target,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Session(String),
    NewSession(String),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDraft {
    owner: Owner,
    text: String,
    start: usize,
    end: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAttempt<P = StoredPrompt> {
    #[serde(default)]
    origin: StoredOrigin,
    runtime: String,
    session: String,
    draft: StoredDraft,
    prompt: P,
    optimistic_part_id: String,
    created: i64,
    admission: StoredAdmission,
}

impl StoredAttempt<TextOnlyPrompt> {
    fn upgrade(self) -> StoredAttempt {
        StoredAttempt {
            origin: self.origin,
            runtime: self.runtime,
            session: self.session,
            draft: self.draft,
            prompt: self.prompt.upgrade(),
            optimistic_part_id: self.optimistic_part_id,
            created: self.created,
            admission: self.admission,
        }
    }
}

impl StoredCreation<TextOnlyPrompt> {
    fn upgrade(self) -> StoredCreation {
        StoredCreation {
            session: self.session,
            project: self.project,
            draft: self.draft,
            prompt: self.prompt.upgrade(),
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
struct StoredPrompt {
    message_id: String,
    text: String,
    files: Vec<StoredFile>,
    agents: Vec<StoredAgent>,
    skills: Vec<String>,
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
    /// Admitted in this order before the prompt.
    context: Vec<StoredContext>,
    model: Option<StoredModel>,
    agent: Option<String>,
    delivery: Option<StoredDelivery>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFile {
    uri: String,
    name: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAgent {
    name: String,
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
struct StoredContext {
    id: String,
    text: String,
    description: Option<String>,
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredModel {
    provider: String,
    model: String,
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
        let parts = &prompt.parts;
        Self {
            message_id: prompt.message_id.to_string(),
            text: prompt.text.clone(),
            files: parts
                .files
                .iter()
                .map(|file| StoredFile {
                    uri: file.uri.clone(),
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
                    text: item.text.clone(),
                    description: item.description.clone(),
                    metadata: item.metadata.as_ref().map(|metadata| metadata.0.clone()),
                })
                .collect(),
            model: prompt.model.as_ref().map(StoredModel::capture),
            agent: prompt.agent.clone(),
            delivery: prompt.delivery.map(|delivery| match delivery {
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
    if value.is_empty() || value.len() > MAX_ID || value.chars().any(char::is_control) {
        return Err(JournalError::Invalid("invalid identifier"));
    }
    Ok(())
}

fn validate_draft(draft: &Draft) -> Result<(), JournalError> {
    if draft.text.len() > MAX_TEXT {
        return Err(JournalError::Invalid("text size limit exceeded"));
    }
    if draft.selection.start > draft.selection.end
        || !draft.text.is_char_boundary(draft.selection.start)
        || !draft.text.is_char_boundary(draft.selection.end)
    {
        return Err(JournalError::Invalid(
            "selection must use ordered UTF-8 boundaries",
        ));
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

impl StoredDraft {
    fn capture(owner: &DraftKey, draft: &Draft) -> Self {
        Self {
            owner: Owner::capture(owner),
            text: draft.text.clone(),
            start: draft.selection.start,
            end: draft.selection.end,
        }
    }

    fn into_draft(self) -> Result<(DraftKey, Draft), JournalError> {
        let owner = self.owner.into_key()?;
        let draft = Draft {
            text: self.text,
            selection: self.start..self.end,
        };
        validate_draft(&draft)?;
        Ok((owner, draft))
    }
}

impl Store {
    fn capture(snapshot: &JournalSnapshot) -> Result<Self, JournalError> {
        validate_snapshot(snapshot)?;
        let attempts = snapshot
            .attempts
            .iter()
            .map(|(key, attempt)| StoredAttempt {
                origin: match attempt.origin {
                    SendOrigin::Existing => StoredOrigin::Existing,
                    SendOrigin::Created => StoredOrigin::Created,
                    SendOrigin::Deleted => StoredOrigin::Deleted,
                },
                runtime: key.runtime.to_string(),
                session: key.session.to_string(),
                draft: StoredDraft::capture(&attempt.owner, &attempt.draft),
                prompt: StoredPrompt::capture(&attempt.prompt),
                optimistic_part_id: attempt.optimistic.parts[0].id.to_string(),
                created: attempt.optimistic.info.created,
                admission: match attempt.admission {
                    Admission::Preparing | Admission::Sending { .. } => StoredAdmission::Sending,
                    Admission::Admitted => StoredAdmission::Admitted,
                    Admission::Uncertain => StoredAdmission::Uncertain,
                },
            })
            .collect();
        Ok(Self {
            version: VERSION,
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
                .map(|(owner, draft)| StoredDraft::capture(owner, draft))
                .collect(),
            attempts,
            creations: snapshot
                .creations
                .iter()
                .map(|(key, attempt)| StoredCreation {
                    session: key.session.to_string(),
                    project: attempt.project.clone(),
                    draft: StoredDraft::capture(&attempt.source, &attempt.draft),
                    prompt: StoredPrompt::capture(&attempt.prompt),
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
        if self.drafts.len() > MAX_ENTRIES
            || self.attempts.len() > MAX_ENTRIES
            || self.creations.len() > MAX_ENTRIES
        {
            return Err(JournalError::Invalid("entry count limit exceeded"));
        }
        let drafts = self
            .drafts
            .into_iter()
            .map(StoredDraft::into_draft)
            .collect::<Result<_, _>>()?;
        let mut attempts = Vec::with_capacity(self.attempts.len());
        for stored in self.attempts {
            let (owner, draft) = stored.draft.into_draft()?;
            let prompt = stored.prompt.into_prompt();
            let optimistic = Message {
                info: MessageInfo {
                    id: prompt.message_id.clone(),
                    role: Role::User,
                    created: stored.created,
                    completed: None,
                    notice: None,
                    assistant: None,
                    compaction: None,
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
                    optimistic,
                    admission: match stored.admission {
                        StoredAdmission::Sending | StoredAdmission::Uncertain => {
                            Admission::Uncertain
                        }
                        StoredAdmission::Admitted => Admission::Admitted,
                    },
                },
            ));
        }
        let mut creations = Vec::with_capacity(self.creations.len());
        for stored in self.creations {
            let (source, draft) = stored.draft.into_draft()?;
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
                    prompt: stored.prompt.into_prompt(),
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
            drafts,
            attempts,
            creations,
            selections: self
                .selections
                .into_iter()
                .map(crate::selection_storage::StoredSelection::restore)
                .collect::<Result<_, _>>()?,
        };
        validate_snapshot(&snapshot)?;
        Ok(snapshot)
    }
}

fn validate_snapshot(snapshot: &JournalSnapshot) -> Result<(), JournalError> {
    crate::selection_storage::validate(&snapshot.selections)?;
    for (owner, state) in &snapshot.selections {
        if let Some(source) = &state.staged_from {
            let valid = snapshot.creations.iter().any(|(key, creation)| {
                creation.source == source.draft
                    && owner.draft.runtime() == &key.runtime
                    && owner.draft.target() == &DraftTarget::Session(key.session.clone())
                    && owner
                        .project
                        .as_ref()
                        .is_some_and(|project| project.as_str() == creation.project)
            });
            if !valid {
                return Err(JournalError::Invalid("staged selection lacks its creation"));
            }
        }
    }
    if snapshot.drafts.len() > MAX_ENTRIES
        || snapshot.attempts.len() > MAX_ENTRIES
        || snapshot.creations.len() > MAX_ENTRIES
    {
        return Err(JournalError::Invalid("entry count limit exceeded"));
    }
    let mut owners = HashSet::new();
    let mut size = 0;
    for (owner, draft) in &snapshot.drafts {
        charge_owner(&mut size, owner)?;
        charge(&mut size, draft.text.len())?;
        Owner::capture(owner).into_key()?;
        validate_draft(draft)?;
        if !owners.insert(owner) {
            return Err(JournalError::Invalid("duplicate draft owner"));
        }
    }
    // A session holds several acknowledged attempts but at most one that is
    // not yet acknowledged, and never both a creation and an attempt.
    let mut sessions = std::collections::HashMap::new();
    let mut messages = HashSet::new();
    let mut parts = HashSet::new();
    for (key, attempt) in &snapshot.attempts {
        charge_owner(&mut size, &attempt.owner)?;
        charge(
            &mut size,
            key.runtime.as_str().len() + key.session.as_str().len(),
        )?;
        charge(&mut size, attempt.draft.text.len())?;
        identifier(key.runtime.as_str())?;
        identifier(key.session.as_str())?;
        Owner::capture(&attempt.owner).into_key()?;
        validate_draft(&attempt.draft)?;
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
        if attempt.prompt.text != attempt.draft.text {
            return Err(JournalError::Invalid(
                "prompt must match the captured draft",
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
        let expected = Message {
            info: MessageInfo {
                id: attempt.prompt.message_id.clone(),
                role: Role::User,
                created: attempt.optimistic.info.created,
                completed: None,
                notice: None,
                assistant: None,
                compaction: None,
            },
            parts: vec![Part::text(part.id.clone(), attempt.prompt.text.clone())],
        };
        if attempt.optimistic != expected {
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
    for (key, attempt) in &snapshot.creations {
        charge_owner(&mut size, &attempt.source)?;
        charge(&mut size, attempt.draft.text.len() + attempt.project.len())?;
        Owner::capture(&attempt.source).into_key()?;
        validate_draft(&attempt.draft)?;
        identifier(&attempt.project)?;
        validate_prompt(&mut size, &key.runtime, &attempt.prompt, &mut messages)?;
        if attempt.source.runtime() != &key.runtime
            || !matches!(attempt.source.target(), DraftTarget::NewSession(_))
            || attempt.request.id() != &key.session
            || attempt.request.directory() != attempt.source.directory()
            || sessions.contains_key(key)
            || !created.insert(key)
            || (attempt.phase != CreationPhase::Deleted && !sources.insert(&attempt.source))
            || attempt.prompt.text != attempt.draft.text
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
    Ok(())
}

/// Bounds and identity checks of one stored prompt. Every prompt and context
/// ID is unique within its runtime across all attempts and creations.
fn validate_prompt<'a>(
    size: &mut usize,
    runtime: &'a RuntimeId,
    prompt: &'a Prompt,
    messages: &mut HashSet<(&'a RuntimeId, &'a MessageId)>,
) -> Result<(), JournalError> {
    if !prompt.is_well_formed() {
        return Err(JournalError::Invalid("malformed prompt"));
    }
    charge(size, prompt.text.len() + prompt.message_id.as_str().len())?;
    if prompt.text.len() > MAX_TEXT {
        return Err(JournalError::Invalid("text size limit exceeded"));
    }
    identifier(prompt.message_id.as_str())?;
    if !messages.insert((runtime, &prompt.message_id)) {
        return Err(JournalError::Invalid("duplicate prompt identity"));
    }
    if let Some(model) = &prompt.model {
        charge(size, model.provider_id.len() + model.model_id.len())?;
        identifier(&model.provider_id)?;
        identifier(&model.model_id)?;
        if let Some(variant) = &model.variant {
            charge(size, variant.len())?;
            identifier(variant)?;
        }
    }
    if let Some(agent) = &prompt.agent {
        charge(size, agent.len())?;
        identifier(agent)?;
    }
    let parts = &prompt.parts;
    if parts.files.len() + parts.agents.len() + parts.skills.len() + parts.context.len()
        > MAX_ENTRIES
    {
        return Err(JournalError::Invalid("entry count limit exceeded"));
    }
    for file in &parts.files {
        charge(size, file.uri.len())?;
        if file.uri.len() > MAX_URI || file.uri.chars().any(char::is_control) {
            return Err(JournalError::Invalid("attachment reference limit exceeded"));
        }
        if let Some(name) = &file.name {
            charge(size, name.len())?;
            identifier(name)?;
        }
    }
    for agent in &parts.agents {
        charge(size, agent.name.len())?;
        identifier(&agent.name)?;
        if let Some(mention) = &agent.mention {
            charge(size, mention.text.len())?;
            if mention.text.len() > MAX_ID {
                return Err(JournalError::Invalid("mention size limit exceeded"));
            }
        }
    }
    for skill in &parts.skills {
        charge(size, skill.id.len())?;
        identifier(&skill.id)?;
    }
    validate_metadata(size, parts.metadata.as_ref())?;
    for item in &parts.context {
        charge(size, item.id.as_str().len() + item.text.len())?;
        identifier(item.id.as_str())?;
        if item.text.len() > MAX_TEXT {
            return Err(JournalError::Invalid("text size limit exceeded"));
        }
        if let Some(description) = &item.description {
            charge(size, description.len())?;
            if description.len() > MAX_TEXT {
                return Err(JournalError::Invalid("text size limit exceeded"));
            }
        }
        validate_metadata(size, item.metadata.as_ref())?;
        if !messages.insert((runtime, &item.id)) {
            return Err(JournalError::Invalid("duplicate context identity"));
        }
    }
    Ok(())
}

fn validate_metadata(size: &mut usize, metadata: Option<&Metadata>) -> Result<(), JournalError> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    let mut bytes = BoundedBytes(Vec::new());
    serde_json::to_writer(&mut bytes, metadata)
        .map_err(|_| JournalError::Invalid("metadata size limit exceeded"))?;
    if bytes.0.len() > MAX_METADATA {
        return Err(JournalError::Invalid("metadata size limit exceeded"));
    }
    charge(size, bytes.0.len())
}

fn charge(size: &mut usize, additional: usize) -> Result<(), JournalError> {
    *size = size.saturating_add(additional);
    if *size as u64 > MAX_FILE {
        return Err(JournalError::Invalid(
            "aggregate snapshot size limit exceeded",
        ));
    }
    Ok(())
}

fn charge_owner(size: &mut usize, owner: &DraftKey) -> Result<(), JournalError> {
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
