//! Normal upstream processing for an explicitly edited local source.
//!
//! Untouched sources bypass this module and keep their delivered text. A caller
//! retains the original immutable capture until the returned capture and any new
//! context identity are durable. This module never consumes a draft or publishes
//! into the workspace. Planning resolves typed paths without reading them. The
//! caller durably owns complete file output bundles before finishing the plan.

use std::fmt;
use std::sync::Arc;

use openchamber_native_core::draft::DraftKey;
use openchamber_native_core::ids::MessageId;
use openchamber_native_core::path;
use openchamber_native_core::send_config::{AgentEntry, AgentMode};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::composer_preparation::{
    ComposerExpansion, ComposerPreparation, ComposerSkill,
};
use openchamber_native_transport::queue_manifest::{
    QUEUE_MANIFEST_ENTRIES, QueueAgent, QueueFileReference, QueueLocalCapture, QueueLocalFile,
    QueueLocalItem, QueueMention, QueuePromptContext, QueueSkill,
};

use crate::attachment_catalog;
use crate::composer_document::{ComposerExcludedReference, ComposerSourceExclusions};
use crate::composer_file_preparation;
use crate::queue_capture::sha256;

/// Content-free failures. The borrowed source remains complete after every error.
#[derive(Debug)]
pub(crate) enum ComposerPreparationError {
    WrongOwner,
    Read,
    #[cfg(test)]
    FilePreparationUnavailable,
    PreparedFilesMismatch,
    InvalidFilePath,
    SkillContextIdentityRequired,
    ExclusionPositionUnavailable,
    InvalidCapture,
}

/// A new typed read request. Existing model-facing file metadata never creates
/// one. Offsets refer to the exact untrimmed display source retained by the plan.
pub(crate) struct ComposerFileRequest {
    pub(crate) mention: QueueMention,
    pub(crate) server_path: String,
}

impl ComposerFileRequest {
    pub(crate) fn mention(&self) -> &QueueMention {
        &self.mention
    }
    pub(crate) fn resolved_path(&self) -> &str {
        &self.server_path
    }
}

impl fmt::Debug for ComposerFileRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ComposerFileRequest([redacted])")
    }
}

/// One source shared across every request. The caller retains its original Arc
/// through publication; the plan grants no file-read or persistence authority.
pub(crate) struct ComposerEditPlan {
    owner: DraftKey,
    source: Arc<QueueLocalCapture>,
    item: QueueLocalItem,
    context_ids: Vec<MessageId>,
    file_requests: Vec<ComposerFileRequest>,
}

impl ComposerEditPlan {
    #[cfg(test)]
    pub(crate) fn owner(&self) -> &DraftKey {
        &self.owner
    }
    pub(crate) fn source(&self) -> &Arc<QueueLocalCapture> {
        &self.source
    }
    pub(crate) fn file_requests(&self) -> &[ComposerFileRequest] {
        &self.file_requests
    }
}

impl fmt::Debug for ComposerEditPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ComposerEditPlan([redacted])")
    }
}

/// An expanded immutable capture still bound to the exact submitted draft owner.
/// The actor must compare this owner before publication and persist the capture
/// before freezing. Retrying a frozen send never invokes preparation again.
pub(crate) struct PreparedComposerCapture {
    owner: DraftKey,
    capture: QueueLocalCapture,
}

impl PreparedComposerCapture {
    pub(crate) fn owner(&self) -> &DraftKey {
        &self.owner
    }
    #[cfg(test)]
    pub(crate) fn capture(&self) -> &QueueLocalCapture {
        &self.capture
    }
    pub(crate) fn into_capture(self) -> QueueLocalCapture {
        self.capture
    }
}

impl fmt::Debug for PreparedComposerCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreparedComposerCapture([redacted])")
    }
}

/// Capture owner, exact server directory and client before awaiting. The caller
/// establishes the client's verified runtime binding; this object has no access
/// to current typing, runtime, session selection, or catalog stores.
pub(crate) struct CapturedComposerPreparation {
    owner: DraftKey,
    server: ComposerPreparation,
}

impl CapturedComposerPreparation {
    pub(crate) fn new(
        owner: DraftKey,
        client: &ServerClient,
        directory: &str,
    ) -> Result<Self, ComposerPreparationError> {
        if path::normalize(directory).as_deref() != Some(owner.directory()) {
            return Err(ComposerPreparationError::WrongOwner);
        }
        let server = ComposerPreparation::new(client, directory)
            .map_err(|_| ComposerPreparationError::Read)?;
        Ok(Self { owner, server })
    }

    /// Convenience path for edits needing no prepared byte bundles. A new byte
    /// request fails explicitly and leaves the caller's source unchanged.
    #[cfg(test)]
    pub(crate) async fn prepare_edit(
        &self,
        source: &QueueLocalCapture,
        skill_context_id: Option<MessageId>,
    ) -> Result<PreparedComposerCapture, ComposerPreparationError> {
        self.prepare_edit_with_exclusions(source, skill_context_id, 0, None)
            .await
    }

    /// The caller passes only this logical source's exclusions. Their original
    /// ordinals have already been filtered by the document owner; preparation
    /// never applies those ordinals to the current arrays or deletes by identity.
    #[cfg(test)]
    pub(crate) async fn prepare_edit_with_exclusions(
        &self,
        source: &QueueLocalCapture,
        skill_context_id: Option<MessageId>,
        source_index: usize,
        exclusions: Option<&ComposerSourceExclusions>,
    ) -> Result<PreparedComposerCapture, ComposerPreparationError> {
        let shared =
            QueueLocalCapture::from_bytes(source.bytes().to_vec(), source.directory(), sha256)
                .map_err(|_| ComposerPreparationError::InvalidCapture)?;
        let plan = self
            .plan_edit_with_exclusions(Arc::new(shared), skill_context_id, source_index, exclusions)
            .await?;
        if !plan.file_requests.is_empty() {
            return Err(ComposerPreparationError::FilePreparationUnavailable);
        }
        self.finish_edit(plan, Vec::new()).await
    }

    pub(crate) async fn plan_edit(
        &self,
        source: Arc<QueueLocalCapture>,
        skill_context_id: Option<MessageId>,
    ) -> Result<ComposerEditPlan, ComposerPreparationError> {
        self.plan_edit_with_exclusions(source, skill_context_id, 0, None)
            .await
    }

    /// Catalogs and path resolution belong to this captured owner. Reference-only
    /// parts require no reads; documents and images produce ordered requests.
    pub(crate) async fn plan_edit_with_exclusions(
        &self,
        source: Arc<QueueLocalCapture>,
        skill_context_id: Option<MessageId>,
        source_index: usize,
        exclusions: Option<&ComposerSourceExclusions>,
    ) -> Result<ComposerEditPlan, ComposerPreparationError> {
        validate_exclusions(source_index, exclusions)?;
        if source.directory() != self.server.directory() {
            return Err(ComposerPreparationError::WrongOwner);
        }
        let text = source.item().content.trim_matches('\n');
        let agents = if text.contains('@') {
            self.server
                .agents()
                .await
                .map_err(|_| ComposerPreparationError::Read)?
        } else {
            Vec::new()
        };
        let tokens = skill_occurrences(&source.item().content);
        let skills = if tokens.is_empty() {
            Vec::new()
        } else {
            self.server
                .skills()
                .await
                .map_err(|_| ComposerPreparationError::Read)?
        };
        let (item, context_ids, file_requests) = process_mentions_with_tokens(
            &source,
            &agents,
            &skills,
            skill_context_id,
            exclusions,
            &tokens,
        )?;
        Ok(ComposerEditPlan {
            owner: self.owner.clone(),
            source,
            item,
            context_ids,
            file_requests,
        })
    }

    /// Accepts one complete ordered bundle per request, already verified and
    /// durably owned by the caller. Only the first document output has an anchor.
    /// Finishing neither reads file bytes nor persists any output again.
    pub(crate) async fn finish_edit(
        &self,
        plan: ComposerEditPlan,
        outputs: Vec<Vec<QueueLocalFile>>,
    ) -> Result<PreparedComposerCapture, ComposerPreparationError> {
        if plan.owner != self.owner || plan.source.directory() != self.server.directory() {
            return Err(ComposerPreparationError::WrongOwner);
        }
        let ComposerEditPlan {
            source,
            mut item,
            context_ids,
            file_requests,
            ..
        } = plan;
        append_prepared_files(&mut item, &file_requests, outputs)?;
        rebase_trimmed_mentions(&mut item);
        // buildOutgoingMessage resolves mentions and skills before expandText.
        // Tokens introduced by a snippet are therefore ordinary expanded prose.
        let expanded = self
            .server
            .expand_text_mapped(&item.text)
            .await
            .map_err(|_| ComposerPreparationError::Read)?;
        apply_expansion(&mut item, expanded);
        let capture = QueueLocalCapture::new(source.directory(), item, context_ids, sha256)
            .map_err(|_| ComposerPreparationError::InvalidCapture)?;
        Ok(PreparedComposerCapture {
            owner: self.owner.clone(),
            capture,
        })
    }
}

#[cfg(test)]
fn process_mentions(
    source: &QueueLocalCapture,
    agents: &[AgentEntry],
    skills: &[ComposerSkill],
    skill_context_id: Option<MessageId>,
) -> Result<(QueueLocalItem, Vec<MessageId>), ComposerPreparationError> {
    process_mentions_excluding(source, agents, skills, skill_context_id, None)
}

#[cfg(test)]
fn process_mentions_excluding(
    source: &QueueLocalCapture,
    agents: &[AgentEntry],
    skills: &[ComposerSkill],
    skill_context_id: Option<MessageId>,
    exclusions: Option<&ComposerSourceExclusions>,
) -> Result<(QueueLocalItem, Vec<MessageId>), ComposerPreparationError> {
    let tokens = skill_occurrences(&source.item().content);
    let (mut item, ids, requests) = process_mentions_with_tokens(
        source,
        agents,
        skills,
        skill_context_id,
        exclusions,
        &tokens,
    )?;
    if !requests.is_empty() {
        return Err(ComposerPreparationError::FilePreparationUnavailable);
    }
    rebase_trimmed_mentions(&mut item);
    Ok((item, ids))
}

fn process_mentions_with_tokens(
    source: &QueueLocalCapture,
    agents: &[AgentEntry],
    skills: &[ComposerSkill],
    skill_context_id: Option<MessageId>,
    exclusions: Option<&ComposerSourceExclusions>,
    tokens: &[SkillToken<'_>],
) -> Result<(QueueLocalItem, Vec<MessageId>, Vec<ComposerFileRequest>), ComposerPreparationError> {
    let mut item = source.item().clone();
    let mut ids = source.context_ids().to_vec();
    let file_requests = plan_file_mentions(source, &mut item, agents, exclusions)?;
    let parsed_agent = match exclusions.filter(|exclusions| !exclusions.agents.is_empty()) {
        Some(exclusions) => mentioned_agent_excluding(source, agents, &exclusions.agents)?,
        None => mentioned_agent(&item.content, agents),
    };
    if let Some(agent) = parsed_agent
        && !item
            .prompt_parts
            .agents
            .iter()
            .any(|saved| saved.name == agent.name)
    {
        item.prompt_parts.agents.push(QueueAgent {
            name: agent.name.clone(),
            mention: None,
        });
    }
    let mut unresolved = Vec::new();
    for token in tokens {
        let Some(skill) = skills.iter().find(|skill| skill.name == token.name) else {
            continue;
        };
        let identity = skill.id.as_deref().unwrap_or(&skill.name);
        if excluded_occurrence(
            source,
            exclusions.map_or(&[][..], |exclusions| exclusions.skills.as_slice()),
            identity,
            &token.mention,
        )? {
            continue;
        }
        if let Some(id) = &skill.id {
            if !item.prompt_parts.skills.iter().any(|saved| &saved.id == id) {
                item.prompt_parts.skills.push(QueueSkill {
                    id: id.clone(),
                    mention: None,
                });
            }
        } else if !unresolved.iter().any(|name| name == token.name) {
            unresolved.push(token.name.to_owned());
        }
    }
    if !unresolved.is_empty() {
        let id = skill_context_id
            .filter(|id| !ids.contains(id))
            .ok_or(ComposerPreparationError::SkillContextIdentityRequired)?;
        item.prompt_parts.context.push(QueuePromptContext {
            id: Some(id.clone()),
            text: skill_instruction(&unresolved),
            description: None,
            metadata: None,
        });
        ids.push(id);
    }
    let text = item.content.trim_matches('\n');
    item.text = if text.chars().all(js_whitespace) {
        String::new()
    } else {
        text.to_owned()
    };
    if entry_count(&item) + file_requests.len() > QUEUE_MANIFEST_ENTRIES {
        return Err(ComposerPreparationError::InvalidCapture);
    }
    Ok((item, ids, file_requests))
}

fn entry_count(item: &QueueLocalItem) -> usize {
    item.files.len()
        + item.prompt_parts.file_references.len()
        + item.prompt_parts.agents.len()
        + item.prompt_parts.skills.len()
        + item.prompt_parts.context.len()
}

fn append_prepared_files(
    item: &mut QueueLocalItem,
    requests: &[ComposerFileRequest],
    outputs: Vec<Vec<QueueLocalFile>>,
) -> Result<(), ComposerPreparationError> {
    if requests.len() != outputs.len()
        || requests.iter().zip(&outputs).any(|(request, files)| {
            files
                .first()
                .is_none_or(|file| file.mention.as_ref() != Some(&request.mention))
                || files.iter().skip(1).any(|file| file.mention.is_some())
        })
    {
        return Err(ComposerPreparationError::PreparedFilesMismatch);
    }
    let added = outputs
        .iter()
        .try_fold(0usize, |count, files| count.checked_add(files.len()))
        .ok_or(ComposerPreparationError::InvalidCapture)?;
    if entry_count(item).saturating_add(added) > QUEUE_MANIFEST_ENTRIES {
        return Err(ComposerPreparationError::InvalidCapture);
    }
    item.files.extend(outputs.into_iter().flatten());
    Ok(())
}

fn validate_exclusions(
    source_index: usize,
    exclusions: Option<&ComposerSourceExclusions>,
) -> Result<(), ComposerPreparationError> {
    let Some(exclusions) = exclusions else {
        return Ok(());
    };
    if exclusions.source != source_index {
        return Err(ComposerPreparationError::InvalidCapture);
    }
    for references in [
        &exclusions.files,
        &exclusions.file_references,
        &exclusions.agents,
        &exclusions.skills,
    ] {
        let mut ordinals = std::collections::HashSet::new();
        if references
            .iter()
            .any(|reference| reference.identity.is_empty() || !ordinals.insert(reference.ordinal))
        {
            return Err(ComposerPreparationError::InvalidCapture);
        }
    }
    Ok(())
}

fn excluded_occurrence(
    source: &QueueLocalCapture,
    exclusions: &[ComposerExcludedReference],
    identity: &str,
    parsed: &QueueMention,
) -> Result<bool, ComposerPreparationError> {
    for exclusion in exclusions {
        let known = exclusion
            .display_mention
            .as_ref()
            .or_else(|| {
                (source.item().content == source.item().text)
                    .then_some(exclusion.original_mention.as_ref())
                    .flatten()
            })
            .filter(|mention| {
                utf16_slice(&source.item().content, mention.start, mention.end)
                    == Some(mention.text.as_str())
            });
        if let Some(known) = known {
            if known == parsed {
                return Ok(true);
            }
        } else if exclusion.identity == identity
            || exclusion
                .original_mention
                .as_ref()
                .is_some_and(|mention| mention.text == parsed.text)
            || exclusion
                .display_mention
                .as_ref()
                .is_some_and(|mention| mention.text == parsed.text)
        {
            return Err(ComposerPreparationError::ExclusionPositionUnavailable);
        }
    }
    Ok(false)
}

fn plan_file_mentions(
    source: &QueueLocalCapture,
    item: &mut QueueLocalItem,
    agents: &[AgentEntry],
    exclusions: Option<&ComposerSourceExclusions>,
) -> Result<Vec<ComposerFileRequest>, ComposerPreparationError> {
    let mut requests = Vec::new();
    for file in file_mentions(&source.item().content, agents) {
        if excluded_occurrence(
            source,
            exclusions.map_or(&[][..], |exclusions| exclusions.files.as_slice()),
            &file.name,
            &file.mention,
        )? {
            continue;
        }
        let server_path = resolve_file_path(source.directory(), &file.name)?;
        let uri = attachment_catalog::file_uri(&server_path);
        let excluded =
            exclusions.map_or(&[][..], |exclusions| exclusions.file_references.as_slice());
        if excluded_file_reference(source, excluded, &uri, &file.mention)? {
            continue;
        }
        let filename = server_path
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .ok_or(ComposerPreparationError::InvalidFilePath)?;
        if filename.len() > 4096 {
            return Err(ComposerPreparationError::InvalidFilePath);
        }
        if composer_file_preparation::reads_on_send(filename) {
            requests.push(ComposerFileRequest {
                mention: file.mention,
                server_path,
            });
        } else if !item
            .prompt_parts
            .file_references
            .iter()
            .any(|saved| saved.uri == uri && saved.mention.as_ref() == Some(&file.mention))
        {
            item.prompt_parts.file_references.push(QueueFileReference {
                uri,
                name: filename.to_owned(),
                description: None,
                mention: Some(file.mention),
            });
        }
        if requests
            .len()
            .saturating_add(item.prompt_parts.file_references.len())
            > QUEUE_MANIFEST_ENTRIES
        {
            return Err(ComposerPreparationError::InvalidCapture);
        }
    }
    Ok(requests)
}

fn excluded_file_reference(
    source: &QueueLocalCapture,
    exclusions: &[ComposerExcludedReference],
    uri: &str,
    mention: &QueueMention,
) -> Result<bool, ComposerPreparationError> {
    for exclusion in exclusions
        .iter()
        .filter(|exclusion| exclusion.identity == uri)
    {
        if excluded_occurrence(source, std::slice::from_ref(exclusion), uri, mention)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn resolve_file_path(directory: &str, name: &str) -> Result<String, ComposerPreparationError> {
    let invalid = || ComposerPreparationError::InvalidFilePath;
    // Keep drive-relative names, URI schemes and root-relative Windows names
    // out of the relative join. All absolute forms use the existing normalizer.
    let bytes = name.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    let absolute = name.starts_with('/') || name.starts_with("\\\\") || drive;
    if absolute {
        return attachment_catalog::remote_path(name).ok_or_else(invalid);
    }
    if name.contains(':') || name.starts_with('\\') {
        return Err(invalid());
    }
    let root = attachment_catalog::remote_path(directory).ok_or_else(invalid)?;
    let name = name.replace('\\', "/");
    let relative = name.strip_prefix("./").unwrap_or(&name);
    if relative.is_empty() {
        return Err(invalid());
    }
    attachment_catalog::remote_path(&format!("{}/{relative}", root.trim_end_matches('/')))
        .ok_or_else(invalid)
}

fn rebase_trimmed_mentions(item: &mut QueueLocalItem) {
    let display = &item.content;
    let text = &item.text;
    let leading_bytes = display.len() - display.trim_start_matches('\n').len();
    let leading_units = u64::try_from(display[..leading_bytes].encode_utf16().count()).ok();
    let mentions = item
        .files
        .iter_mut()
        .map(|file| &mut file.mention)
        .chain(
            item.prompt_parts
                .file_references
                .iter_mut()
                .map(|file| &mut file.mention),
        )
        .chain(
            item.prompt_parts
                .agents
                .iter_mut()
                .map(|agent| &mut agent.mention),
        )
        .chain(
            item.prompt_parts
                .skills
                .iter_mut()
                .map(|skill| &mut skill.mention),
        );
    for mention in mentions {
        let Some(anchor) = mention else {
            continue;
        };
        let mapped = leading_units.and_then(|leading| {
            if utf16_slice(display, anchor.start, anchor.end) != Some(anchor.text.as_str()) {
                return None;
            }
            let start = anchor.start.checked_sub(leading)?;
            let end = anchor.end.checked_sub(leading)?;
            (utf16_slice(text, start, end) == Some(anchor.text.as_str())).then_some((start, end))
        });
        match mapped {
            Some((start, end)) => {
                anchor.start = start;
                anchor.end = end;
            }
            None => *mention = None,
        }
    }
}

fn utf16_slice(text: &str, start: u64, end: u64) -> Option<&str> {
    if start >= end {
        return None;
    }
    let start = utf16_boundary(text, start)?;
    let end = utf16_boundary(text, end)?;
    text.get(start..end)
}

fn utf16_boundary(text: &str, offset: u64) -> Option<usize> {
    let mut units = 0u64;
    for (byte, character) in text.char_indices() {
        if units == offset {
            return Some(byte);
        }
        units = units.checked_add(character.len_utf16() as u64)?;
        if units > offset {
            return None;
        }
    }
    (units == offset).then_some(text.len())
}

fn apply_expansion(item: &mut QueueLocalItem, expanded: ComposerExpansion) {
    let ranges = expanded.source_ranges();
    let mentions = item
        .files
        .iter_mut()
        .map(|file| &mut file.mention)
        .chain(
            item.prompt_parts
                .file_references
                .iter_mut()
                .map(|file| &mut file.mention),
        )
        .chain(
            item.prompt_parts
                .agents
                .iter_mut()
                .map(|agent| &mut agent.mention),
        )
        .chain(
            item.prompt_parts
                .skills
                .iter_mut()
                .map(|skill| &mut skill.mention),
        );
    for mention in mentions {
        let Some(anchor) = mention else {
            continue;
        };
        let index = ranges.partition_point(|range| range.input_end <= anchor.start);
        let mapped = ranges
            .get(index)
            .filter(|range| range.input_start <= anchor.start && anchor.end <= range.input_end)
            .and_then(|range| {
                let start = range
                    .output_start
                    .checked_add(anchor.start.checked_sub(range.input_start)?)?;
                let end = range
                    .output_start
                    .checked_add(anchor.end.checked_sub(range.input_start)?)?;
                Some((start, end))
            });
        match mapped {
            Some((start, end)) => {
                anchor.start = start;
                anchor.end = end;
            }
            None => *mention = None,
        }
    }
    item.text = expanded.into_text();
}

/// parseAgentMentions visits non-primary agents in catalog order and preserves
/// the text, even when the matching mention appears later in the document.
fn mentioned_agent<'a>(text: &str, agents: &'a [AgentEntry]) -> Option<&'a AgentEntry> {
    agents
        .iter()
        .filter(|agent| {
            !agent.hidden && matches!(agent.mode, Some(AgentMode::Subagent | AgentMode::All))
        })
        .find(|agent| {
            text.char_indices().any(|(start, character)| {
                character == '@' && agent_mention_at(text, start, agent).is_some()
            })
        })
}

fn mentioned_agent_excluding<'a>(
    source: &QueueLocalCapture,
    agents: &'a [AgentEntry],
    exclusions: &[ComposerExcludedReference],
) -> Result<Option<&'a AgentEntry>, ComposerPreparationError> {
    let text = &source.item().content;
    for agent in agents {
        if agent.hidden || !matches!(agent.mode, Some(AgentMode::Subagent | AgentMode::All)) {
            continue;
        }
        for (start, character) in text.char_indices() {
            if character != '@' {
                continue;
            }
            let Some(mention) = agent_mention_at(text, start, agent) else {
                continue;
            };
            if !excluded_occurrence(source, exclusions, &agent.name, &mention)? {
                return Ok(Some(agent));
            }
        }
    }
    Ok(None)
}

fn agent_mention_at(text: &str, start: usize, agent: &AgentEntry) -> Option<QueueMention> {
    if !agent_boundary(text[..start].chars().next_back()) {
        return None;
    }
    let suffix = &text[start + 1..];
    let mut actual = suffix.chars();
    for expected in agent.name.chars() {
        if js_case(actual.next()?) != js_case(expected) {
            return None;
        }
    }
    let end = start + 1 + suffix.len() - actual.as_str().len();
    // JavaScript's non-Unicode regexp \b compares ASCII word classes.
    if agent.name.chars().next_back().is_some_and(ascii_word)
        == actual.next().is_some_and(ascii_word)
    {
        return None;
    }
    Some(textual_mention(text, start, end))
}

fn agent_boundary(character: Option<char>) -> bool {
    character
        .is_none_or(|character| js_whitespace(character) || "()[]{}\"'`,.;:".contains(character))
}

fn js_case(character: char) -> char {
    let mut upper = character.to_uppercase();
    let first = upper
        .next()
        .expect("a Unicode uppercase mapping is nonempty");
    if upper.next().is_some() || !character.is_ascii() && first.is_ascii() {
        character
    } else {
        first
    }
}

fn ascii_word(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

#[cfg(test)]
fn has_file_mention(text: &str, agents: &[AgentEntry]) -> bool {
    !file_mentions(text, agents).is_empty()
}

struct FileMention {
    name: String,
    mention: QueueMention,
}

fn file_mentions(text: &str, agents: &[AgentEntry]) -> Vec<FileMention> {
    let mut mentions = Vec::new();
    let mut remaining = text;
    let mut offset = 0;
    let mut units = 0u64;
    let count_units = |text: &str| {
        let count = text.encode_utf16().count() as u64;
        #[cfg(test)]
        record_mention_scan_work(count);
        count
    };
    while let Some(local_start) = remaining.find('@') {
        let start = offset + local_start;
        units += count_units(&remaining[..local_start]);
        let end = text[start + 1..]
            .find(js_whitespace)
            .map_or(text.len(), |end| start + 1 + end);
        let before = text[..start].chars().next_back();
        let boundary = before.is_none_or(|character| {
            js_whitespace(character) || "()[]{}<>\"'`,.;:".contains(character)
        });
        let name = text[start + 1..end]
            .trim_start_matches(['`', '"', '\'', '<', '(', '[', '{'])
            .trim_end_matches([
                ')', ']', '}', ',', '.', ';', ':', '!', '?', '`', '"', '\'', '>',
            ]);
        if boundary
            && !name.contains(['<', '>'])
            && !agents
                .iter()
                .any(|agent| !agent.hidden && agent.name.to_lowercase() == name.to_lowercase())
            && name.contains(['/', '\\', '.'])
        {
            let raw_name = &text[start + 1..end];
            let cleaned_start = raw_name.len()
                - raw_name
                    .trim_start_matches(['`', '"', '\'', '<', '(', '[', '{'])
                    .len();
            let mention_end = start + 1 + cleaned_start + name.len();
            let value = &text[start..mention_end];
            let end_units = units + count_units(value);
            mentions.push(FileMention {
                name: name.to_owned(),
                mention: QueueMention {
                    start: units,
                    end: end_units,
                    text: value.to_owned(),
                },
            });
            units = end_units + count_units(&text[mention_end..end]);
        } else {
            units += count_units(&text[start..end]);
        }
        // scanMentions consumes the full non-whitespace token, including any @
        // inside it. A bare @ consumes no following whitespace.
        offset = end;
        remaining = &text[end..];
    }
    // The final search also visits a suffix with no @. Charge that disjoint
    // suffix so the test counter covers the complete source, including prose.
    count_units(remaining);
    mentions
}

struct SkillToken<'a> {
    name: &'a str,
    mention: QueueMention,
}

#[cfg(test)]
thread_local! {
    static MENTION_SCAN_BUDGET: std::cell::Cell<Option<(u64, u64)>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn record_mention_scan_work(units: u64) {
    MENTION_SCAN_BUDGET.with(|budget| {
        if let Some((used, limit)) = budget.get() {
            let used = used + units;
            budget.set(Some((used, limit)));
            assert!(
                used <= limit,
                "mention scan exceeded its UTF-16 work budget: {used} > {limit}"
            );
        }
    });
}

/// Whether preparation could change `text`: an `@` agent or file mention, a
/// `$skill` token or a `#snippet` token. Text without them prepares to itself.
pub(crate) fn text_needs_preparation(text: &str) -> bool {
    text.contains('@')
        || openchamber_native_transport::composer_preparation::has_snippet_token(text)
        || !skill_occurrences(text).is_empty()
}

fn skill_occurrences(text: &str) -> Vec<SkillToken<'_>> {
    let mut tokens = Vec::new();
    let mut units = 0u64;
    let mut previous = None;
    for (index, character) in text.char_indices() {
        #[cfg(test)]
        record_mention_scan_work(character.len_utf16() as u64);
        let start = units;
        units += character.len_utf16() as u64;
        let boundary = previous.is_none_or(js_whitespace);
        previous = Some(character);
        if !matches!(character, '$' | '¥' | '￥' | '＄') || !boundary {
            continue;
        }
        let suffix = &text[index + character.len_utf8()..];
        if !suffix
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        {
            continue;
        }
        let length = suffix
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(byte))
            .count();
        let name = &suffix[..length];
        tokens.push(SkillToken {
            name,
            mention: QueueMention {
                start,
                end: units + length as u64,
                text: text[index..index + character.len_utf8() + length].to_owned(),
            },
        });
    }
    tokens
}

fn textual_mention(text: &str, start: usize, end: usize) -> QueueMention {
    let offset = text[..start].encode_utf16().count() as u64;
    #[cfg(test)]
    record_mention_scan_work(offset);
    let value = &text[start..end];
    QueueMention {
        start: offset,
        end: offset + value.encode_utf16().count() as u64,
        text: value.to_owned(),
    }
}

fn skill_instruction(names: &[String]) -> String {
    let formatted = names
        .iter()
        .map(|name| format!("${name}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "The user explicitly mentioned these skills in their message: {formatted}. Use the corresponding skill tool when it is relevant to accomplishing the user's request."
    )
}

fn js_whitespace(character: char) -> bool {
    matches!(character, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

#[cfg(test)]
#[path = "composer_preparation_tests.rs"]
mod tests;
