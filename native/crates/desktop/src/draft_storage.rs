//! Strict journal representation and validation of complete composer captures.

use std::collections::HashSet;

use openchamber_native_core::draft::{Draft, DraftKey};
use openchamber_native_core::draft_content::{
    AgentMention, DraftContent, DraftContext, DraftFile, DraftFileContent, Mention, SkillRef,
};
use openchamber_native_core::ids::{AttachmentId, ContextItemId};
use openchamber_native_transport::prompt::Prompt;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::mutation_journal::{
    JournalError, MAX_ENTRIES, MAX_ID, MAX_METADATA, MAX_TEXT, MAX_URI, Owner, charge, identifier,
};

/// Makes nullable v7 fields required: absence must never erase captured content.
pub(crate) fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredDraft<S = String> {
    pub(crate) owner: Owner,
    pub(crate) text: S,
    pub(crate) start: usize,
    pub(crate) end: usize,
    content: StoredDraftContent<S>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum StoredDraftContent<S> {
    Owned(StoredContent<S>),
    CapturedPrompt,
}

/// The physical content representation selected by the enclosing stored draft.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ContentStorage {
    Owned,
    CapturedPrompt,
}

/// Versions 1 through 6 never stored unsent semantic content.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyDraft {
    owner: Owner,
    text: String,
    start: usize,
    end: usize,
}

impl LegacyDraft {
    pub(crate) fn upgrade(self, prompt: Option<&Prompt>) -> StoredDraft {
        StoredDraft {
            owner: self.owner,
            text: self.text,
            start: self.start,
            end: self.end,
            content: if prompt.is_some() {
                StoredDraftContent::CapturedPrompt
            } else {
                StoredDraftContent::Owned(StoredContent::capture(&DraftContent::default()))
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredContent<S> {
    files: Vec<StoredFile<S>>,
    agents: Vec<StoredAgent>,
    skills: Vec<String>,
    #[serde(deserialize_with = "required_option")]
    metadata_json: Option<S>,
    context: Vec<StoredContext<S>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFile<S> {
    id: String,
    #[serde(deserialize_with = "required_option")]
    name: Option<String>,
    #[serde(deserialize_with = "required_option")]
    media_type: Option<String>,
    #[serde(deserialize_with = "required_option")]
    byte_len: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    source_document_id: Option<String>,
    content: StoredFileContent<S>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum StoredFileContent<S> {
    Reference { uri: S },
    Prepared,
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
    metadata_json: Option<S>,
    #[serde(deserialize_with = "required_option")]
    instructions: Option<S>,
}

impl StoredDraft {
    #[cfg(test)]
    pub(crate) fn capture(owner: &DraftKey, draft: &Draft) -> Self {
        Self::capture_with(owner, draft, &[], str::to_owned)
    }

    /// Both durable saves and prospective admission use this exact v7 layout.
    pub(crate) fn capture_with<'a, S>(
        owner: &DraftKey,
        draft: &'a Draft,
        added_files: &'a [DraftFile],
        text: impl Fn(&'a str) -> S + Copy,
    ) -> StoredDraft<S> {
        StoredDraft {
            owner: Owner::capture(owner),
            text: text(&draft.text),
            start: draft.selection.start,
            end: draft.selection.end,
            content: StoredDraftContent::Owned(StoredContent::capture_with(
                &draft.content,
                added_files,
                text,
            )),
        }
    }

    pub(crate) fn capture_attempt_with<'a, S>(
        owner: &DraftKey,
        draft: &'a Draft,
        prompt: &Prompt,
        text: impl Fn(&'a str) -> S + Copy,
    ) -> StoredDraft<S> {
        if !matches_recovered_content(owner.runtime().as_str(), draft, prompt) {
            return Self::capture_with(owner, draft, &[], text);
        }
        StoredDraft {
            owner: Owner::capture(owner),
            text: text(&draft.text),
            start: draft.selection.start,
            end: draft.selection.end,
            content: StoredDraftContent::CapturedPrompt,
        }
    }

    pub(crate) fn into_draft(self) -> Result<(DraftKey, Draft), JournalError> {
        self.restore(None)
    }

    pub(crate) fn into_capture(self, prompt: &Prompt) -> Result<(DraftKey, Draft), JournalError> {
        self.restore(Some(prompt))
    }

    fn restore(self, prompt: Option<&Prompt>) -> Result<(DraftKey, Draft), JournalError> {
        let owner = self.owner.into_key()?;
        let content = match self.content {
            StoredDraftContent::Owned(content) => content.restore(),
            StoredDraftContent::CapturedPrompt => {
                let prompt = prompt.ok_or(JournalError::Invalid(
                    "unbound draft cannot reference captured prompt content",
                ))?;
                recover_content(owner.runtime().as_str(), prompt, &self.text)
            }
        };
        let draft = Draft {
            text: self.text,
            selection: self.start..self.end,
            content,
        };
        Ok((owner, draft))
    }
}

impl<S> StoredDraft<S> {
    pub(crate) fn storage(&self) -> ContentStorage {
        match &self.content {
            StoredDraftContent::Owned(_) => ContentStorage::Owned,
            StoredDraftContent::CapturedPrompt => ContentStorage::CapturedPrompt,
        }
    }
}

impl StoredContent<String> {
    fn capture(content: &DraftContent) -> Self {
        Self::capture_with(content, &[], str::to_owned)
    }

    fn capture_with<'a, S>(
        content: &'a DraftContent,
        added_files: &'a [DraftFile],
        text: impl Fn(&'a str) -> S + Copy,
    ) -> StoredContent<S> {
        StoredContent {
            files: content
                .files
                .iter()
                .chain(added_files)
                .map(|file| StoredFile {
                    id: file.id.to_string(),
                    name: file.name.clone(),
                    media_type: file.media_type.clone(),
                    byte_len: file.byte_len,
                    source_document_id: file.source_document_id.as_ref().map(ToString::to_string),
                    content: match &file.content {
                        DraftFileContent::Reference { uri } => {
                            StoredFileContent::Reference { uri: text(uri) }
                        }
                        DraftFileContent::Prepared => StoredFileContent::Prepared,
                    },
                })
                .collect(),
            agents: content
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
            skills: content
                .skills
                .iter()
                .map(|skill| skill.id.clone())
                .collect(),
            metadata_json: content.metadata_json.as_deref().map(text),
            context: content
                .context
                .iter()
                .map(|item| StoredContext {
                    id: item.id.to_string(),
                    text: text(&item.text),
                    description: item.description.as_deref().map(text),
                    metadata_json: item.metadata_json.as_deref().map(text),
                    instructions: item.instructions.as_deref().map(text),
                })
                .collect(),
        }
    }

    fn restore(self) -> DraftContent {
        DraftContent {
            files: self
                .files
                .into_iter()
                .map(|file| DraftFile {
                    id: AttachmentId::from(file.id),
                    name: file.name,
                    media_type: file.media_type,
                    byte_len: file.byte_len,
                    source_document_id: file.source_document_id.map(AttachmentId::from),
                    content: match file.content {
                        StoredFileContent::Reference { uri } => DraftFileContent::Reference { uri },
                        StoredFileContent::Prepared => DraftFileContent::Prepared,
                    },
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
            metadata_json: self.metadata_json,
            context: self
                .context
                .into_iter()
                .map(|item| DraftContext {
                    id: ContextItemId::from(item.id),
                    text: item.text,
                    description: item.description,
                    metadata_json: item.metadata_json,
                    instructions: item.instructions,
                })
                .collect(),
        }
    }
}

/// Selects the same storage representation as migration without materializing a
/// second copy of every prompt context item during prospective admission.
fn matches_recovered_content(runtime: &str, draft: &Draft, prompt: &Prompt) -> bool {
    let content = &draft.content;
    let parts = &prompt.parts;
    if content.files.len() != parts.files.len()
        || content.context.len() != parts.context.len()
        || content.skills != parts.skills
        || !matches_metadata(content.metadata_json.as_deref(), parts.metadata.as_ref())
    {
        return false;
    }
    for (index, (file, part)) in content.files.iter().zip(&parts.files).enumerate() {
        if file.id.as_str() != legacy_id(runtime, prompt.message_id.as_str(), "file", index)
            || file.name != part.name
            || file.media_type.is_some()
            || file.byte_len.is_some()
            || file.source_document_id.is_some()
            || !matches!(&file.content, DraftFileContent::Reference { uri } if uri == &part.uri)
        {
            return false;
        }
    }
    let mut agents = content.agents.iter();
    let mut seen = HashSet::new();
    for agent in &parts.agents {
        let mention = agent
            .mention
            .as_ref()
            .filter(|mention| mention_matches(&draft.text, mention));
        let key = (
            agent.name.as_str(),
            mention.map(|mention| (mention.start, mention.end, mention.text.as_str())),
        );
        if seen.insert(key)
            && !agents.next().is_some_and(|captured| {
                captured.name == agent.name && captured.mention.as_ref() == mention
            })
        {
            return false;
        }
    }
    if agents.next().is_some() {
        return false;
    }
    for (index, (item, part)) in content.context.iter().zip(&parts.context).enumerate() {
        if item.id.as_str() != legacy_id(runtime, prompt.message_id.as_str(), "context", index)
            || item.text != part.text
            || item.description != part.description
            || item.instructions.is_some()
            || !matches_metadata(item.metadata_json.as_deref(), part.metadata.as_ref())
        {
            return false;
        }
    }
    true
}

fn matches_metadata(
    captured: Option<&str>,
    metadata: Option<&openchamber_native_transport::prompt_parts::Metadata>,
) -> bool {
    match (captured, metadata) {
        (None, None) => true,
        (Some(captured), Some(metadata)) => {
            serde_json::to_string(&metadata.0).is_ok_and(|encoded| captured == encoded)
        }
        _ => false,
    }
}

/// Stable migration labels use the original message identity and ordered slot.
/// References stay references. An anchor survives only when it matches the source
/// draft; the frozen prompt separately retains every original mention verbatim.
fn recover_content(runtime: &str, prompt: &Prompt, source_text: &str) -> DraftContent {
    let parts = &prompt.parts;
    let mut agents = Vec::new();
    let mut seen_agents = HashSet::new();
    for agent in &parts.agents {
        let mention = agent
            .mention
            .as_ref()
            .filter(|mention| mention_matches(source_text, mention));
        let key = (
            agent.name.as_str(),
            mention.map(|mention| (mention.start, mention.end, mention.text.as_str())),
        );
        if seen_agents.insert(key) {
            agents.push(AgentMention {
                name: agent.name.clone(),
                mention: mention.cloned(),
            });
        }
    }
    DraftContent {
        files: parts
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| DraftFile {
                id: AttachmentId::from(legacy_id(
                    runtime,
                    prompt.message_id.as_str(),
                    "file",
                    index,
                )),
                name: file.name.clone(),
                media_type: None,
                byte_len: None,
                source_document_id: None,
                content: DraftFileContent::Reference {
                    uri: file.uri.clone(),
                },
            })
            .collect(),
        agents,
        skills: parts.skills.clone(),
        metadata_json: parts.metadata.as_ref().map(|metadata| {
            serde_json::to_string(&metadata.0).expect("JSON object serialization cannot fail")
        }),
        context: parts
            .context
            .iter()
            .enumerate()
            .map(|(index, item)| DraftContext {
                id: ContextItemId::from(legacy_id(
                    runtime,
                    prompt.message_id.as_str(),
                    "context",
                    index,
                )),
                text: item.text.clone(),
                description: item.description.clone(),
                metadata_json: item.metadata.as_ref().map(|metadata| {
                    serde_json::to_string(&metadata.0)
                        .expect("JSON object serialization cannot fail")
                }),
                instructions: None,
            })
            .collect(),
    }
}

/// Whether ordered UTF-16 offsets select the mention's exact source text.
pub(crate) fn mention_matches(text: &str, mention: &Mention) -> bool {
    if mention.start > mention.end {
        return false;
    }
    let mut units = 0u32;
    let mut start = (mention.start == 0).then_some(0);
    let mut end = (mention.end == 0).then_some(0);
    for (offset, character) in text.char_indices() {
        units += character.len_utf16() as u32;
        let boundary = offset + character.len_utf8();
        if units == mention.start {
            start = Some(boundary);
        }
        if units == mention.end {
            end = Some(boundary);
        }
        if units >= mention.end {
            break;
        }
    }
    match (start, end) {
        (Some(start), Some(end)) => text.get(start..end) == Some(mention.text.as_str()),
        _ => false,
    }
}

fn legacy_id(runtime: &str, message: &str, kind: &str, index: usize) -> String {
    use std::fmt::Write;

    let mut hash = Sha256::new();
    hash.update((runtime.len() as u64).to_le_bytes());
    hash.update(runtime.as_bytes());
    hash.update(message.as_bytes());
    hash.update([0]);
    hash.update(kind.as_bytes());
    hash.update((index as u64).to_le_bytes());
    let mut id = format!("legacy-{kind}-");
    for byte in hash.finalize() {
        write!(&mut id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    id
}

pub(crate) fn validate(size: &mut usize, draft: &Draft) -> Result<(), JournalError> {
    validate_with_files(size, draft, &[])
}

pub(crate) fn validate_with_files(
    size: &mut usize,
    draft: &Draft,
    added_files: &[DraftFile],
) -> Result<(), JournalError> {
    let mut logical_size = 0;
    validate_fields(&mut logical_size, draft, added_files)?;
    charge(size, logical_size)
}

fn validate_fields(
    size: &mut usize,
    draft: &Draft,
    added_files: &[DraftFile],
) -> Result<(), JournalError> {
    text(size, &draft.text)?;
    if draft.selection.start > draft.selection.end
        || !draft.text.is_char_boundary(draft.selection.start)
        || !draft.text.is_char_boundary(draft.selection.end)
    {
        return Err(JournalError::Invalid(
            "selection must use ordered UTF-8 boundaries",
        ));
    }
    let content = &draft.content;
    if content.files.len()
        + added_files.len()
        + content.agents.len()
        + content.skills.len()
        + content.context.len()
        > MAX_ENTRIES
    {
        return Err(JournalError::Invalid("content entry count limit exceeded"));
    }
    let mut files = HashSet::new();
    for file in content.files.iter().chain(added_files) {
        identifier(file.id.as_str())?;
        count(size, file.id.as_str().len())?;
        if !files.insert(&file.id) {
            return Err(JournalError::Invalid("duplicate attachment identity"));
        }
        if let Some(name) = &file.name {
            identifier(name)?;
            count(size, name.len())?;
        }
        if let Some(media_type) = &file.media_type {
            identifier(media_type)?;
            let valid_token = |token: &str| {
                !token.is_empty()
                    && token
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&byte))
            };
            if !media_type
                .split_once('/')
                .is_some_and(|(kind, subtype)| valid_token(kind) && valid_token(subtype))
            {
                return Err(JournalError::Invalid("invalid attachment media type"));
            }
            count(size, media_type.len())?;
        }
        if let Some(source) = &file.source_document_id {
            identifier(source.as_str())?;
            count(size, source.as_str().len())?;
            if source == &file.id {
                return Err(JournalError::Invalid(
                    "attachment is its own source document",
                ));
            }
        }
        match &file.content {
            DraftFileContent::Reference { uri } => {
                if uri.trim().is_empty() || uri.len() > MAX_URI || uri.chars().any(char::is_control)
                {
                    return Err(JournalError::Invalid("invalid attachment reference"));
                }
                count(size, uri.len())?;
            }
            DraftFileContent::Prepared => {
                if file.name.is_none() || file.media_type.is_none() || file.byte_len.is_none() {
                    return Err(JournalError::Invalid(
                        "prepared attachment lacks representation facts",
                    ));
                }
            }
        }
    }
    validate_agent_fields(size, &content.agents, &draft.text)?;
    let mut skills = HashSet::new();
    for skill in &content.skills {
        identifier(&skill.id)?;
        count(size, skill.id.len())?;
        if !skills.insert(&skill.id) {
            return Err(JournalError::Invalid("duplicate skill identity"));
        }
    }
    metadata(size, content.metadata_json.as_deref())?;
    let mut context = HashSet::new();
    for item in &content.context {
        identifier(item.id.as_str())?;
        count(size, item.id.as_str().len())?;
        if !context.insert(&item.id) {
            return Err(JournalError::Invalid("duplicate draft context identity"));
        }
        text(size, &item.text)?;
        for value in [&item.description, &item.instructions]
            .into_iter()
            .flatten()
        {
            text(size, value)?;
        }
        metadata(size, item.metadata_json.as_deref())?;
    }
    Ok(())
}

/// All logical fields are validated, while aggregate charging follows the exact
/// representation selected by capture or read from the enclosing stored draft.
pub(crate) fn validate_capture(
    size: &mut usize,
    draft: &Draft,
    storage: ContentStorage,
) -> Result<(), JournalError> {
    let mut logical_size = 0;
    validate_fields(&mut logical_size, draft, &[])?;
    match storage {
        ContentStorage::Owned => charge(size, logical_size),
        ContentStorage::CapturedPrompt => charge(size, draft.text.len()),
    }
}

pub(crate) fn validate_agents(
    size: &mut usize,
    agents: &[AgentMention],
    prompt_text: &str,
) -> Result<(), JournalError> {
    let mut logical_size = 0;
    validate_agent_fields(&mut logical_size, agents, prompt_text)?;
    charge(size, logical_size)
}

fn validate_agent_fields(
    size: &mut usize,
    agents: &[AgentMention],
    prompt_text: &str,
) -> Result<(), JournalError> {
    let mut agents_seen = HashSet::new();
    let mut offsets = HashSet::new();
    for agent in agents {
        identifier(&agent.name)?;
        count(size, agent.name.len())?;
        let mention_key = agent
            .mention
            .as_ref()
            .map(|mention| (mention.start, mention.end, mention.text.as_str()));
        if !agents_seen.insert((agent.name.as_str(), mention_key)) {
            return Err(JournalError::Invalid("duplicate agent reference"));
        }
        if let Some(mention) = &agent.mention {
            if mention.start > mention.end || mention.text.len() > MAX_ID {
                return Err(JournalError::Invalid("invalid mention offsets"));
            }
            offsets.insert(mention.start);
            offsets.insert(mention.end);
            count(size, mention.text.len())?;
        }
    }
    offsets.remove(&0);
    let mut units = 0u32;
    for character in prompt_text.chars() {
        if offsets.is_empty() {
            break;
        }
        units += character.len_utf16() as u32;
        offsets.remove(&units);
    }
    if !offsets.is_empty() {
        return Err(JournalError::Invalid("invalid mention offsets"));
    }
    Ok(())
}

fn text(size: &mut usize, text: &str) -> Result<(), JournalError> {
    if text.len() > MAX_TEXT {
        return Err(JournalError::Invalid("text size limit exceeded"));
    }
    count(size, text.len())
}

fn metadata(size: &mut usize, metadata: Option<&str>) -> Result<(), JournalError> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    if metadata.len() > MAX_METADATA {
        return Err(JournalError::Invalid("metadata size limit exceeded"));
    }
    let value: serde_json::Value = serde_json::from_str(metadata)
        .map_err(|_| JournalError::Invalid("malformed metadata JSON"))?;
    if !value.is_object() {
        return Err(JournalError::Invalid("metadata must be a JSON object"));
    }
    count(size, metadata.len())
}

fn count(size: &mut usize, additional: usize) -> Result<(), JournalError> {
    *size = size
        .checked_add(additional)
        .ok_or(JournalError::Invalid("logical draft size overflow"))?;
    Ok(())
}

#[cfg(test)]
mod borrowed_capture_tests {
    use super::*;
    use openchamber_native_core::draft::DraftTarget;
    use openchamber_native_core::ids::{MessageId, RuntimeId, SessionId};
    use openchamber_native_transport::prompt_parts::{PromptParts, SyntheticContext};

    fn owner() -> DraftKey {
        DraftKey::new(
            RuntimeId::from("runtime"),
            "/project",
            DraftTarget::Session(SessionId::from("session")),
        )
        .expect("canonical test owner")
    }

    #[test]
    fn prospective_capture_borrows_large_text_context_and_instructions() {
        let owner = owner();
        let mut draft = Draft::new("t".repeat(MAX_TEXT));
        draft.content.context.push(DraftContext {
            id: ContextItemId::from("context"),
            text: "c".repeat(MAX_TEXT),
            description: Some("d".repeat(MAX_TEXT)),
            metadata_json: Some("{\"retained\":true}".to_owned()),
            instructions: Some("i".repeat(MAX_TEXT)),
        });
        let stored = StoredDraft::capture_with(&owner, &draft, &[], |text| text);
        assert_eq!(
            stored.text.as_ptr(),
            draft.text.as_ptr(),
            "preflight borrows the source text"
        );
        let StoredDraftContent::Owned(content) = &stored.content else {
            panic!("independent draft owns its semantic content")
        };
        let source = &draft.content.context[0];
        let captured = &content.context[0];
        assert_eq!(captured.text.as_ptr(), source.text.as_ptr());
        assert_eq!(
            captured.description.expect("description").as_ptr(),
            source
                .description
                .as_ref()
                .expect("source description")
                .as_ptr(),
        );
        assert_eq!(
            captured.instructions.expect("instructions").as_ptr(),
            source
                .instructions
                .as_ref()
                .expect("source instructions")
                .as_ptr(),
        );
        assert_eq!(
            serde_json::to_vec(&stored).expect("borrowed v7 encoding"),
            serde_json::to_vec(&StoredDraft::capture(&owner, &draft)).expect("owned v7 encoding"),
            "ownership cannot change the canonical serialized layout",
        );
    }

    #[test]
    fn borrowed_storage_selection_matches_exact_migration_recovery() {
        let owner = owner();
        let prompt = Prompt {
            message_id: MessageId::from("msg_recovery"),
            text: "@plan source".to_owned(),
            parts: PromptParts {
                agents: vec![
                    AgentMention {
                        name: "plan".to_owned(),
                        mention: Some(Mention {
                            start: 0,
                            end: 5,
                            text: "@plan".to_owned(),
                        }),
                    },
                    AgentMention {
                        name: "plan".to_owned(),
                        mention: Some(Mention {
                            start: 0,
                            end: 5,
                            text: "@plan".to_owned(),
                        }),
                    },
                    AgentMention {
                        name: "detached".to_owned(),
                        mention: Some(Mention {
                            start: 0,
                            end: 3,
                            text: "wrong".to_owned(),
                        }),
                    },
                ],
                context: vec![SyntheticContext {
                    id: MessageId::from("msg_context"),
                    text: "c".repeat(MAX_TEXT),
                    description: Some("description".to_owned()),
                    metadata: None,
                }],
                ..PromptParts::default()
            },
            model: None,
            agent: None,
            delivery: None,
        };
        let mut draft = Draft::new(prompt.text.clone());
        draft.content = recover_content(owner.runtime().as_str(), &prompt, &draft.text);
        let captured = StoredDraft::capture_attempt_with(&owner, &draft, &prompt, |text| text);
        assert!(matches!(captured.storage(), ContentStorage::CapturedPrompt));
        assert_eq!(captured.text.as_ptr(), draft.text.as_ptr());
        for change in 0..4 {
            let mut changed = draft.clone();
            match change {
                0 => changed.content.context[0].text.push('!'),
                1 => changed.content.context[0].instructions = Some("retain".to_owned()),
                2 => changed.content.agents.reverse(),
                _ => changed.content.metadata_json = Some("{}".to_owned()),
            }
            assert_ne!(
                changed.content,
                recover_content(owner.runtime().as_str(), &prompt, &changed.text),
            );
            assert!(matches!(
                StoredDraft::capture_attempt_with(&owner, &changed, &prompt, |text| text).storage(),
                ContentStorage::Owned,
            ));
        }
    }
}
