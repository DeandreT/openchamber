//! Explicit prepared-byte ownership and retained staging targets for the journal.
//!
//! Validation is purely lexical. No load scans, reads, writes or deletes blobs,
//! and no retained target grants authority to contact an unverified runtime.

use std::collections::{HashMap, HashSet};
use std::fmt;

use openchamber_native_core::draft::{Draft, DraftKey};
use openchamber_native_core::draft_content::{DraftFile, DraftFileContent};
use openchamber_native_core::ids::{AttachmentId, RuntimeId};
use openchamber_native_core::path;
use openchamber_native_transport::attachment::{AttachmentUpload, MAX_PREPARED_ATTACHMENT_BYTES};
use openchamber_native_transport::prompt_parts::PromptParts;
use serde::{Deserialize, Serialize};

use crate::attachment_spool::{BlobDescriptor, BlobId};
use crate::draft_storage::required_option;
use crate::mutation_journal::{
    JournalError, MAX_DIRECTORY, MAX_ENTRIES, MAX_URI, Owner, charge, charge_owner, identifier,
};

pub(crate) type AttachmentCatalog = HashMap<AttachmentId, PreparedAttachment>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparedAttachment {
    pub blob: BlobDescriptor,
    pub staging: Option<StagingDescriptor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StagingPhase {
    Planned,
    Writing,
    Ready,
    Uncertain,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StagingDescriptor {
    pub runtime: RuntimeId,
    pub directory: String,
    pub temporary_directory: String,
    pub server_path: String,
    pub uri: String,
    pub filename: String,
    pub phase: StagingPhase,
}

impl fmt::Debug for StagingDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StagingDescriptor")
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PreparationHold {
    pub owner: DraftKey,
    pub id: AttachmentId,
    pub generation: u64,
    pub blob: BlobId,
}

impl fmt::Debug for PreparationHold {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparationHold")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredAttachment {
    id: String,
    blob: StoredBlob,
    #[serde(deserialize_with = "required_option")]
    staging: Option<StoredStaging>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBlob {
    id: String,
    byte_len: u64,
    sha256: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredStaging {
    runtime: String,
    directory: String,
    temporary_directory: String,
    server_path: String,
    uri: String,
    filename: String,
    phase: StagingPhase,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredPreparation {
    owner: Owner,
    id: String,
    generation: u64,
    blob: String,
}

pub(crate) fn capture(catalog: &AttachmentCatalog) -> Vec<StoredAttachment> {
    let mut entries: Vec<_> = catalog.iter().collect();
    entries.sort_by_key(|(id, _)| *id);
    entries
        .into_iter()
        .map(|(id, attachment)| StoredAttachment {
            id: id.to_string(),
            blob: StoredBlob {
                id: attachment.blob.id().as_str().to_owned(),
                byte_len: attachment.blob.byte_len(),
                sha256: *attachment.blob.sha256(),
            },
            staging: attachment.staging.as_ref().map(|staging| StoredStaging {
                runtime: staging.runtime.to_string(),
                directory: staging.directory.clone(),
                temporary_directory: staging.temporary_directory.clone(),
                server_path: staging.server_path.clone(),
                uri: staging.uri.clone(),
                filename: staging.filename.clone(),
                phase: staging.phase,
            }),
        })
        .collect()
}

pub(crate) fn restore(entries: Vec<StoredAttachment>) -> Result<AttachmentCatalog, JournalError> {
    if entries.len() > MAX_ENTRIES {
        return Err(JournalError::Invalid(
            "attachment catalog count limit exceeded",
        ));
    }
    let mut catalog = HashMap::new();
    for entry in entries {
        let id = AttachmentId::from(entry.id);
        let attachment = PreparedAttachment {
            blob: BlobDescriptor::new(
                blob_id(&entry.blob.id)?,
                entry.blob.byte_len,
                entry.blob.sha256,
            ),
            staging: entry.staging.map(|staging| StagingDescriptor {
                runtime: RuntimeId::from(staging.runtime),
                directory: staging.directory,
                temporary_directory: staging.temporary_directory,
                server_path: staging.server_path,
                uri: staging.uri,
                filename: staging.filename,
                phase: staging.phase,
            }),
        };
        if catalog.insert(id, attachment).is_some() {
            return Err(JournalError::Invalid(
                "duplicate catalog attachment identity",
            ));
        }
    }
    Ok(catalog)
}

impl StoredPreparation {
    pub(crate) fn capture(hold: &PreparationHold) -> Self {
        Self {
            owner: Owner::capture(&hold.owner),
            id: hold.id.to_string(),
            generation: hold.generation,
            blob: hold.blob.as_str().to_owned(),
        }
    }

    pub(crate) fn restore(self) -> Result<PreparationHold, JournalError> {
        Ok(PreparationHold {
            owner: self.owner.into_key()?,
            id: AttachmentId::from(self.id),
            generation: self.generation,
            blob: blob_id(&self.blob)?,
        })
    }
}

pub(crate) fn blob_id(value: &str) -> Result<BlobId, JournalError> {
    BlobId::parse(value).map_err(|_| JournalError::Invalid("invalid blob identity"))
}

pub(crate) fn validate(
    size: &mut usize,
    catalog: &AttachmentCatalog,
    preparations: &[PreparationHold],
    retired: &[BlobId],
) -> Result<(), JournalError> {
    if catalog.len() > MAX_ENTRIES
        || preparations.len() > MAX_ENTRIES
        || retired.len() > MAX_ENTRIES
    {
        return Err(JournalError::Invalid(
            "attachment ownership count limit exceeded",
        ));
    }
    let mut targets = HashSet::new();
    for (id, attachment) in catalog {
        identifier(id.as_str())?;
        if id.as_str() != attachment.blob.id().as_str()
            || attachment.blob.byte_len() > MAX_PREPARED_ATTACHMENT_BYTES as u64
        {
            return Err(JournalError::Invalid("invalid prepared blob association"));
        }
        charge(
            size,
            id.as_str().len() + attachment.blob.id().as_str().len() + 32 + 8,
        )?;
        if let Some(staging) = &attachment.staging {
            validate_staging(size, staging)?;
            let target = remote_path(&staging.server_path)
                .ok_or(JournalError::Invalid("invalid staging target"))?;
            if !targets.insert((&staging.runtime, target)) {
                return Err(JournalError::Invalid("duplicate staging target"));
            }
        }
    }
    let mut holds = HashSet::new();
    for hold in preparations {
        Owner::capture(&hold.owner).into_key()?;
        charge_owner(size, &hold.owner)?;
        identifier(hold.id.as_str())?;
        charge(size, hold.id.as_str().len() + hold.blob.as_str().len() + 8)?;
        if hold.id.as_str() != hold.blob.as_str() || !holds.insert(hold.id.as_str()) {
            return Err(JournalError::Invalid(
                "invalid or duplicate preparation hold",
            ));
        }
        // A hold precedes the durable put receipt. Only that receipt creates a
        // catalog descriptor; the proposed label still protects partial bytes.
        if let Some(attachment) = catalog.get(&hold.id) {
            if attachment.blob.id() != &hold.blob {
                return Err(JournalError::Invalid(
                    "preparation differs from catalog blob",
                ));
            }
            validate_owner(&hold.owner, attachment)?;
        }
    }
    let mut retired_ids = HashSet::new();
    for blob in retired {
        charge(size, blob.as_str().len())?;
        if catalog.contains_key(blob.as_str())
            || holds.contains(blob.as_str())
            || !retired_ids.insert(blob)
        {
            return Err(JournalError::Invalid(
                "retired blob remains live or is duplicated",
            ));
        }
    }
    Ok(())
}

/// A resource vector describes the outgoing payload independently of its rollback draft.
pub(crate) fn validate_resources(
    size: &mut usize,
    catalog: &AttachmentCatalog,
    owner: &DraftKey,
    resources: &[AttachmentId],
) -> Result<(), JournalError> {
    if resources.len() > MAX_ENTRIES {
        return Err(JournalError::Invalid("resource count limit exceeded"));
    }
    let mut ids = HashSet::new();
    for id in resources {
        identifier(id.as_str())?;
        charge(size, id.as_str().len())?;
        let attachment = catalog
            .get(id)
            .ok_or(JournalError::Invalid("resource lacks catalog attachment"))?;
        if !ids.insert(id) {
            return Err(JournalError::Invalid("duplicate resource identity"));
        }
        validate_owner(owner, attachment)?;
    }
    Ok(())
}

pub(crate) fn validate_draft<'a>(
    catalog: &AttachmentCatalog,
    owner: &DraftKey,
    draft: &'a Draft,
    immutable_files: &mut HashMap<&'a AttachmentId, &'a DraftFile>,
) -> Result<(), JournalError> {
    for file in &draft.content.files {
        if let Some(previous) = immutable_files.insert(&file.id, file)
            && previous != file
        {
            return Err(JournalError::Invalid(
                "attachment identity has conflicting immutable content",
            ));
        }
        match &file.content {
            DraftFileContent::Prepared => {
                let attachment = catalog.get(&file.id).ok_or(JournalError::Invalid(
                    "prepared file lacks catalog attachment",
                ))?;
                if file.byte_len != Some(attachment.blob.byte_len()) {
                    return Err(JournalError::Invalid(
                        "prepared file length differs from blob",
                    ));
                }
                if let Some(staging) = &attachment.staging
                    && file.name.as_deref() != Some(staging.filename.as_str())
                {
                    return Err(JournalError::Invalid(
                        "prepared file differs from staging filename",
                    ));
                }
                validate_owner(owner, attachment)?;
            }
            DraftFileContent::Reference { .. } if catalog.contains_key(&file.id) => {
                return Err(JournalError::Invalid(
                    "reference identity aliases prepared bytes",
                ));
            }
            DraftFileContent::Reference { .. } => {}
        }
    }
    Ok(())
}

/// Converted starter parts keep each prepared source's captured, ready target.
/// Generic references require no catalog entry and never trigger a resource read.
pub(crate) fn validate_captured_parts(
    catalog: &AttachmentCatalog,
    owner: &DraftKey,
    draft: &Draft,
    parts: &PromptParts,
) -> Result<(), JournalError> {
    for file in &draft.content.files {
        if !matches!(&file.content, DraftFileContent::Prepared) {
            continue;
        }
        let attachment = catalog.get(&file.id).ok_or(JournalError::Invalid(
            "starter source lacks prepared attachment",
        ))?;
        validate_owner(owner, attachment)?;
        let staging = attachment
            .staging
            .as_ref()
            .filter(|staging| staging.phase == StagingPhase::Ready)
            .ok_or(JournalError::Invalid(
                "starter source lacks a ready staging target",
            ))?;
        if !parts
            .files
            .iter()
            .any(|part| part.uri == staging.uri && part.name == file.name)
        {
            return Err(JournalError::Invalid(
                "starter parts omit a prepared source target",
            ));
        }
    }
    Ok(())
}

fn validate_owner(owner: &DraftKey, attachment: &PreparedAttachment) -> Result<(), JournalError> {
    if let Some(staging) = &attachment.staging
        && (owner.runtime() != &staging.runtime
            || remote_path(&staging.directory).as_deref() != Some(owner.directory()))
    {
        return Err(JournalError::Invalid(
            "staging belongs to another runtime or directory",
        ));
    }
    Ok(())
}

fn validate_staging(size: &mut usize, staging: &StagingDescriptor) -> Result<(), JournalError> {
    identifier(staging.runtime.as_str())?;
    AttachmentUpload::new(&staging.filename, Vec::new())
        .map_err(|_| JournalError::Invalid("invalid staging filename"))?;
    remote_path(&staging.directory).ok_or(JournalError::Invalid("invalid staging directory"))?;
    let root = remote_path(&staging.temporary_directory)
        .ok_or(JournalError::Invalid("invalid staging temporary root"))?;
    let target =
        remote_path(&staging.server_path).ok_or(JournalError::Invalid("invalid staging target"))?;
    let prefix = format!("{}/openchamber/", root.trim_end_matches('/'));
    let relative = target
        .strip_prefix(&prefix)
        .and_then(|relative| relative.split_once('/'));
    let valid = relative.is_some_and(|(folder, name)| {
        folder.len() == 16
            && folder
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && name == staging.filename
    });
    if !valid
        || staging.server_path.ends_with(['/', '\\'])
        || staging.uri.len() > MAX_URI
        || staging.uri != file_uri(&staging.server_path)
    {
        return Err(JournalError::Invalid(
            "invalid retained staging target or URI",
        ));
    }
    charge(
        size,
        staging.runtime.as_str().len()
            + staging.directory.len()
            + staging.temporary_directory.len()
            + staging.server_path.len()
            + staging.uri.len()
            + staging.filename.len(),
    )
}

// Mirrors transport attachment restoration's lexical checks. Its validator is
// private and restoring a plan also performs a network read, which journal loads
// must never do. Filename validation above reuses its public, pure upload boundary.
/// Normalizes an absolute server path without traversal or filesystem access.
pub(crate) fn remote_path(value: &str) -> Option<String> {
    if value.len() > MAX_DIRECTORY
        || value.trim() != value
        || value.chars().any(char::is_control)
        || (value.starts_with('/') && !value.starts_with("//") && value.contains('\\'))
        || (value.starts_with('\\') && !value.starts_with("\\\\"))
    {
        return None;
    }
    let normalized = path::normalize(value)?;
    let bytes = normalized.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';
    let remainder = if drive {
        &normalized[3..]
    } else if let Some(unc) = normalized.strip_prefix("//") {
        if unc.starts_with(['?', '.']) || unc.split('/').count() < 2 {
            return None;
        }
        unc
    } else {
        normalized.strip_prefix('/')?
    };
    if !remainder.is_empty()
        && remainder
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
    {
        return None;
    }
    Some(normalized)
}

/// Matches the transport queue's file URL encoding without touching a local path.
pub(crate) fn file_uri(server_path: &str) -> String {
    use std::fmt::Write;

    let normalized = server_path.replace('\\', "/");
    let drive = normalized.as_bytes().get(1) == Some(&b':')
        && normalized.as_bytes()[0].is_ascii_alphabetic();
    let mut uri = if drive { "file:///" } else { "file://" }.to_owned();
    for (index, byte) in normalized.bytes().enumerate() {
        if byte.is_ascii_alphanumeric()
            || b"/-_.!~*'()".contains(&byte)
            || (drive && index == 1 && byte == b':')
        {
            uri.push(char::from(byte));
        } else {
            write!(&mut uri, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    uri
}
