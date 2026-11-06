//! Client-owned composer drafts keyed by their captured identity.
//!
//! A draft belongs to the runtime, actual directory and session (or
//! new-session draft) that was current when it was typed. Switching sessions
//! saves the outgoing draft under that captured key, so a late save can
//! never land in whichever session is selected by then.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;

pub use crate::draft_content::ContentConflict;
use crate::draft_content::{AgentMention, DraftContent, DraftContext, DraftFile};
use crate::ids::{DraftId, RuntimeId, SessionId};
use crate::path;

/// What a draft will be sent to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DraftTarget {
    Session(SessionId),
    /// A new session that does not exist yet.
    NewSession(DraftId),
}

/// The identity a draft is stored under.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DraftKey {
    runtime: RuntimeId,
    directory: String,
    target: DraftTarget,
}

impl DraftKey {
    /// Builds a key from the session's real directory.
    ///
    /// Returns `None` when the directory is unusable.
    pub fn new(runtime: RuntimeId, directory: &str, target: DraftTarget) -> Option<Self> {
        Some(Self {
            runtime,
            directory: path::normalize(directory)?,
            target,
        })
    }

    pub fn runtime(&self) -> &RuntimeId {
        &self.runtime
    }

    pub fn directory(&self) -> &str {
        &self.directory
    }

    pub fn target(&self) -> &DraftTarget {
        &self.target
    }
}

/// Composer text, semantic content and selection; caret offsets are UTF-8 bytes.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Draft {
    pub text: String,
    pub selection: Range<usize>,
    pub content: DraftContent,
}

impl fmt::Debug for Draft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Draft")
            .field("text", &"[redacted]")
            .field("selection", &self.selection)
            .field("content", &self.content)
            .finish()
    }
}

impl Draft {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        Self {
            text,
            selection: end..end,
            content: DraftContent::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.content.is_empty()
    }

    /// Whether the draft has text, attachments or context that can be submitted.
    pub fn has_sendable_content(&self) -> bool {
        !self.text.trim().is_empty()
            || !self.content.files.is_empty()
            || !self.content.context.is_empty()
    }

    /// Compares every retained value except the caret and selection.
    pub fn same_content(&self, other: &Self) -> bool {
        self.text == other.text && self.content == other.content
    }

    /// Applies the editor's text and UTF-8 selection to this owned draft.
    ///
    /// Mentions survive only in the unambiguous common prefix or suffix of one
    /// inferred replacement. Replaced or invalid UTF-16 anchors become semantic
    /// agent references. Files, context, skills and metadata remain unchanged.
    pub fn edit_text(&mut self, text: String, selection: Range<usize>) {
        if self.text != text
            && self
                .content
                .agents
                .iter()
                .any(|agent| agent.mention.is_some())
        {
            let mut prefix = 0;
            for (old, new) in self.text.chars().zip(text.chars()) {
                if old != new {
                    break;
                }
                prefix += old.len_utf8();
            }
            let mut suffix = 0;
            for (old, new) in self.text.chars().rev().zip(text.chars().rev()) {
                if old != new {
                    break;
                }
                suffix += old.len_utf8();
            }
            // Overlapping matches admit several insertion/deletion locations.
            // Only text outside all of those locations has a proven identity.
            let shorter = self.text.len().min(text.len());
            let certain_prefix = prefix.min(shorter - suffix);
            let certain_suffix = suffix.min(shorter - prefix);
            let old_suffix = self.text.len() - certain_suffix;
            let new_suffix = text.len() - certain_suffix;
            let old_units = self.text[..old_suffix].encode_utf16().count();
            let new_units = text[..new_suffix].encode_utf16().count();
            let boundaries = mention_boundaries(&self.text, &self.content.agents);
            let mut detached = false;
            for agent in &mut self.content.agents {
                let Some(mention) = &mut agent.mention else {
                    continue;
                };
                let start = boundaries.get(&mention.start).copied();
                let end = boundaries.get(&mention.end).copied();
                let survives = match (start, end) {
                    (Some(start), Some(end))
                        if self.text.get(start..end) == Some(mention.text.as_str()) =>
                    {
                        if end <= certain_prefix {
                            true
                        } else if start >= old_suffix {
                            match (
                                shifted_mention_offset(mention.start, old_units, new_units),
                                shifted_mention_offset(mention.end, old_units, new_units),
                            ) {
                                (Some(start), Some(end)) => {
                                    mention.start = start;
                                    mention.end = end;
                                    true
                                }
                                _ => false,
                            }
                        } else {
                            false
                        }
                    }
                    _ => false,
                };
                if !survives {
                    agent.mention = None;
                    detached = true;
                }
            }
            if detached {
                // Several replaced mentions can become the same semantic agent.
                let mut names = HashSet::new();
                self.content
                    .agents
                    .retain(|agent| agent.mention.is_some() || names.insert(agent.name.clone()));
            }
        }
        self.text = text;
        self.selection = selection;
    }

    /// Consumes captured values only while their text still matches this draft.
    ///
    /// Files and context require full-value equality, so a newer payload with the
    /// same identity survives. New textual mentions refuse the whole consumption
    /// because clearing their text would invalidate their ranges. `false` leaves
    /// this draft unchanged; the caller retains ownership of the captured attempt.
    #[must_use]
    pub fn consume_captured(&mut self, captured: &Self) -> bool {
        if self.text != captured.text
            || self
                .content
                .agents
                .iter()
                .any(|agent| agent.mention.is_some() && !captured.content.agents.contains(agent))
        {
            return false;
        }
        self.text.clear();
        self.selection = 0..0;
        self.content
            .files
            .retain(|file| !captured.content.files.contains(file));
        self.content
            .agents
            .retain(|agent| !captured.content.agents.contains(agent));
        self.content
            .skills
            .retain(|skill| !captured.content.skills.contains(skill));
        self.content
            .context
            .retain(|item| !captured.content.context.contains(item));
        if self.content.metadata_json == captured.content.metadata_json {
            self.content.metadata_json = None;
        }
        true
    }
}

/// Resolves all requested UTF-16 endpoints in one pass over the old text.
fn mention_boundaries(text: &str, agents: &[AgentMention]) -> HashMap<u32, usize> {
    let mut requested: HashSet<u32> = agents
        .iter()
        .filter_map(|agent| agent.mention.as_ref())
        .flat_map(|mention| [mention.start, mention.end])
        .collect();
    let mut boundaries = HashMap::new();
    if requested.remove(&0) {
        boundaries.insert(0, 0);
    }
    let mut units = 0u32;
    for (offset, character) in text.char_indices() {
        if requested.is_empty() {
            break;
        }
        let Some(next) = units.checked_add(character.len_utf16() as u32) else {
            break;
        };
        units = next;
        if requested.remove(&units) {
            boundaries.insert(units, offset + character.len_utf8());
        }
    }
    boundaries
}

fn shifted_mention_offset(offset: u32, old_units: usize, new_units: usize) -> Option<u32> {
    let tail = usize::try_from(offset).ok()?.checked_sub(old_units)?;
    u32::try_from(new_units.checked_add(tail)?).ok()
}

/// Restores a failed capture without replacing newer typing or semantic content.
///
/// Distinct non-empty texts join with a blank line, and failed mentions move by
/// that prefix's UTF-16 length. Equal texts stay single. Mixed drafts retain the
/// current selection. Conflicts leave both inputs untouched, so the retained
/// attempt remains the owner until the caller can resolve it explicitly.
pub fn restore_failed(current: Option<&Draft>, failed: &Draft) -> Result<Draft, ContentConflict> {
    let Some(current) = current.filter(|draft| !draft.is_empty()) else {
        return Ok(failed.clone());
    };
    let metadata_json = match (
        &current.content.metadata_json,
        &failed.content.metadata_json,
    ) {
        (Some(current), Some(failed)) if current != failed => {
            return Err(ContentConflict::Metadata);
        }
        (Some(metadata), _) | (_, Some(metadata)) => Some(metadata.clone()),
        (None, None) => None,
    };
    let append = !current.text.is_empty() && !failed.text.is_empty() && current.text != failed.text;
    let prefix = if append
        && failed
            .content
            .agents
            .iter()
            .any(|agent| agent.mention.is_some())
    {
        u32::try_from(current.text.encode_utf16().count())
            .ok()
            .and_then(|length| length.checked_add(2))
            .ok_or(ContentConflict::MentionOffsetOverflow)?
    } else {
        0
    };
    let mut agents = current.content.agents.clone();
    for agent in &failed.content.agents {
        let mut agent = agent.clone();
        if let Some(mention) = &mut agent.mention {
            mention.start = mention
                .start
                .checked_add(prefix)
                .ok_or(ContentConflict::MentionOffsetOverflow)?;
            mention.end = mention
                .end
                .checked_add(prefix)
                .ok_or(ContentConflict::MentionOffsetOverflow)?;
        }
        if !agents.contains(&agent) {
            agents.push(agent);
        }
    }
    let mut skills = current.content.skills.clone();
    for skill in &failed.content.skills {
        if !skills.contains(skill) {
            skills.push(skill.clone());
        }
    }
    let text = if append {
        format!("{}\n\n{}", current.text, failed.text)
    } else if current.text.is_empty() {
        failed.text.clone()
    } else {
        current.text.clone()
    };
    Ok(Draft {
        text,
        selection: current.selection.clone(),
        content: DraftContent {
            files: merge_files(&current.content.files, &failed.content.files)?,
            agents,
            skills,
            metadata_json,
            context: merge_context(&current.content.context, &failed.content.context)?,
        },
    })
}

fn merge_files(
    current: &[DraftFile],
    failed: &[DraftFile],
) -> Result<Vec<DraftFile>, ContentConflict> {
    let mut files: Vec<DraftFile> = Vec::new();
    let mut indexes = HashMap::new();
    for file in current.iter().chain(failed) {
        if let Some(&index) = indexes.get(&file.id) {
            if files[index] != *file {
                return Err(ContentConflict::File {
                    id: file.id.clone(),
                });
            }
        } else {
            indexes.insert(&file.id, files.len());
            files.push(file.clone());
        }
    }
    Ok(files)
}

fn merge_context(
    current: &[DraftContext],
    failed: &[DraftContext],
) -> Result<Vec<DraftContext>, ContentConflict> {
    let mut context: Vec<DraftContext> = Vec::new();
    let mut indexes = HashMap::new();
    for item in current.iter().chain(failed) {
        if let Some(&index) = indexes.get(&item.id) {
            if context[index] != *item {
                return Err(ContentConflict::Context {
                    id: item.id.clone(),
                });
            }
        } else {
            indexes.insert(&item.id, context.len());
            context.push(item.clone());
        }
    }
    Ok(context)
}

/// All unsent drafts of the client.
///
/// Retention never discards unsent content: only an explicit clear or send
/// removes a non-empty draft.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DraftStore {
    drafts: HashMap<DraftKey, Draft>,
}

impl DraftStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `draft` under `key`; an empty draft clears the entry.
    pub fn save(&mut self, key: DraftKey, draft: Draft) {
        if draft.is_empty() {
            self.drafts.remove(&key);
        } else {
            self.drafts.insert(key, draft);
        }
    }

    pub fn get(&self, key: &DraftKey) -> Option<&Draft> {
        self.drafts.get(key)
    }

    /// Borrow every retained draft for a complete persistence snapshot.
    pub fn iter(&self) -> impl Iterator<Item = (&DraftKey, &Draft)> {
        self.drafts.iter()
    }

    /// Removes and returns the draft, as a send does.
    pub fn take(&mut self, key: &DraftKey) -> Option<Draft> {
        self.drafts.remove(key)
    }

    /// Moves a new-session draft to the session it became.
    ///
    /// An existing draft under the session key wins, so a newer draft is
    /// never overwritten by a materialization that arrives late.
    pub fn materialize(&mut self, from: &DraftKey, session_id: SessionId) -> Option<DraftKey> {
        let draft = self.drafts.remove(from)?;
        let key = DraftKey {
            runtime: from.runtime.clone(),
            directory: from.directory.clone(),
            target: DraftTarget::Session(session_id),
        };
        self.drafts.entry(key.clone()).or_insert(draft);
        Some(key)
    }

    pub fn len(&self) -> usize {
        self.drafts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.drafts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft_content::{AgentMention, DraftFileContent, Mention, SkillRef};
    use crate::ids::{AttachmentId, ContextItemId};

    fn file(id: &str) -> DraftFile {
        DraftFile {
            id: AttachmentId::from(id),
            name: Some(format!("{id}.txt")),
            media_type: Some("text/plain".to_owned()),
            byte_len: Some(3),
            source_document_id: None,
            content: DraftFileContent::Prepared,
        }
    }

    fn context(id: &str) -> DraftContext {
        DraftContext {
            id: ContextItemId::from(id),
            text: format!("context {id}"),
            description: Some("selection".to_owned()),
            metadata_json: Some("{\"source\":\"selection\"}".to_owned()),
            instructions: Some("Read this selection".to_owned()),
        }
    }

    fn agent(name: &str, start: u32, end: u32) -> AgentMention {
        AgentMention {
            name: name.to_owned(),
            mention: Some(Mention {
                start,
                end,
                text: format!("@{name}"),
            }),
        }
    }

    fn key(runtime: &str, directory: &str, session: &str) -> DraftKey {
        DraftKey::new(
            RuntimeId::from(runtime),
            directory,
            DraftTarget::Session(SessionId::from(session)),
        )
        .expect("directory is usable")
    }

    #[test]
    fn drafts_stay_with_their_captured_runtime_and_directory() {
        let mut store = DraftStore::new();
        store.save(key("a", "/p/", "s1"), Draft::new("hello"));
        assert_eq!(
            store
                .get(&key("a", "/p", "s1"))
                .map(|draft| draft.text.as_str()),
            Some("hello")
        );
        assert_eq!(store.get(&key("b", "/p", "s1")), None);
        assert_eq!(store.get(&key("a", "/q", "s1")), None);
    }

    #[test]
    fn saving_an_empty_draft_clears_it() {
        let mut store = DraftStore::new();
        store.save(key("a", "/p", "s1"), Draft::new("x"));
        store.save(key("a", "/p", "s1"), Draft::default());
        assert!(store.is_empty());
    }

    #[test]
    fn attachment_and_context_only_drafts_remain_retained_and_sendable() {
        let mut store = DraftStore::new();
        let mut attachment_only = Draft::default();
        attachment_only.content.files.push(file("file"));
        let mut context_only = Draft::default();
        context_only.content.context.push(context("context"));
        for (session, draft) in [("files", attachment_only), ("context", context_only)] {
            assert!(!draft.is_empty());
            assert!(draft.has_sendable_content());
            let owner = key("a", "/p", session);
            store.save(owner.clone(), draft.clone());
            assert_eq!(store.get(&owner), Some(&draft));
        }
    }

    #[test]
    fn references_and_metadata_are_retained_without_making_a_draft_sendable() {
        let mut draft = Draft::new(" \n");
        draft.content.agents.push(AgentMention {
            name: "plan".to_owned(),
            mention: None,
        });
        draft.content.skills.push(SkillRef {
            id: "skill".to_owned(),
        });
        draft.content.metadata_json = Some("opaque JSON".to_owned());
        assert!(!draft.is_empty());
        assert!(!draft.has_sendable_content());
        let mut store = DraftStore::new();
        let owner = key("a", "/p", "s1");
        draft.text.clear();
        store.save(owner.clone(), draft.clone());
        assert_eq!(store.get(&owner), Some(&draft));
    }

    #[test]
    fn same_content_ignores_only_the_selection() {
        let draft = Draft::new("text");
        let mut moved_caret = draft.clone();
        moved_caret.selection = 1..2;
        assert!(draft.same_content(&moved_caret));
        moved_caret.content.files.push(file("new"));
        assert!(!draft.same_content(&moved_caret));
        let mut changed_context = draft.clone();
        changed_context.content.context.push(context("new"));
        assert!(!draft.same_content(&changed_context));
    }

    #[test]
    fn text_edits_keep_only_anchors_outside_the_replacement() {
        for (old, new, start, expected) in [
            ("old @plan tail", "newer @plan tail", 4, Some((6, 11))),
            ("@plan tail", "@plan changed", 0, Some((0, 5))),
            ("@plan", "x", 0, None),
            ("@plan", "@plXan", 0, None),
            ("@plan", "@pln", 0, None),
            ("@plan", "@x", 0, None),
            ("@plan tail", "xail", 0, None),
            ("@plan", "😀@plan", 0, Some((2, 7))),
            ("@plan", "@plan😀", 0, Some((0, 5))),
            ("😀@plan", "@plan", 2, Some((0, 5))),
            ("😀 @plan", "😃 @plan", 3, Some((3, 8))),
            ("é @plan", "e\u{301} @plan", 2, Some((3, 8))),
            ("@plan", "@pla\u{301}n", 0, None),
            ("@plan", "@@plan", 0, None),
            ("@plan", "😀@plan!", 0, None),
            ("@plan", "", 0, None),
        ] {
            let mut draft = Draft::new(old);
            draft.content.agents.push(agent("plan", start, start + 5));
            let selection = new.len()..new.len();
            draft.edit_text(new.to_owned(), selection.clone());
            assert_eq!(draft.text, new);
            assert_eq!(draft.selection, selection);
            assert_eq!(draft.content.agents[0].name, "plan");
            assert_eq!(
                draft.content.agents[0]
                    .mention
                    .as_ref()
                    .map(|mention| (mention.start, mention.end)),
                expected,
                "anchor survival must follow the changed character range"
            );
        }
    }

    #[test]
    fn text_edits_preserve_unicode_mention_text_at_character_boundaries() {
        for (name, end) in [("😀", 3), ("e\u{301}", 3)] {
            let mut draft = Draft::new(format!("@{name}"));
            draft.content.agents.push(agent(name, 0, end));
            draft.edit_text(format!("🧠 @{name}"), 4..4);
            assert_eq!(draft.content.agents, vec![agent(name, 3, end + 3)]);
            draft.edit_text(format!("🧠 @{name}!"), 4..4);
            assert_eq!(draft.content.agents, vec![agent(name, 3, end + 3)]);
            assert_eq!(
                draft.selection,
                4..4,
                "selection stays in editor byte units"
            );
        }
    }

    #[test]
    fn text_edits_detach_invalid_and_split_surrogate_anchors() {
        for mention in [
            Mention {
                start: 1,
                end: 8,
                text: "@plan".to_owned(),
            },
            Mention {
                start: 0,
                end: 1,
                text: "😀".to_owned(),
            },
            Mention {
                start: 3,
                end: 9,
                text: "@plan".to_owned(),
            },
            Mention {
                start: 8,
                end: 3,
                text: "@plan".to_owned(),
            },
            Mention {
                start: 3,
                end: 8,
                text: "@build".to_owned(),
            },
        ] {
            let mut draft = Draft::new("😀 @plan");
            draft.content.agents.push(AgentMention {
                name: "plan".to_owned(),
                mention: Some(mention),
            });
            draft.edit_text("😀 @plan!".to_owned(), 10..10);
            assert_eq!(
                draft.content.agents,
                vec![AgentMention {
                    name: "plan".to_owned(),
                    mention: None,
                }]
            );
        }
    }

    #[test]
    fn repeated_mentions_keep_distinct_proven_anchors_and_detach_ambiguous_ones() {
        let detached = AgentMention {
            name: "plan".to_owned(),
            mention: None,
        };
        let mut original = Draft::new("@plan @plan");
        original.content.agents = vec![agent("plan", 0, 5), agent("plan", 6, 11)];
        for (text, expected) in [
            (
                "@plan @plan",
                vec![agent("plan", 0, 5), agent("plan", 6, 11)],
            ),
            (
                "😀 @plan @plan",
                vec![agent("plan", 3, 8), agent("plan", 9, 14)],
            ),
            ("x @plan", vec![detached.clone(), agent("plan", 2, 7)]),
            ("@plan x", vec![agent("plan", 0, 5), detached.clone()]),
            ("@plan", vec![detached.clone()]),
            ("x", vec![detached.clone()]),
        ] {
            let mut draft = original.clone();
            draft.edit_text(text.to_owned(), 0..0);
            assert_eq!(draft.content.agents, expected);
        }
        let mut draft = Draft::new("@plan");
        draft.content.agents = vec![agent("plan", 0, 5), detached.clone()];
        draft.edit_text("@plan @plan".to_owned(), 11..11);
        assert_eq!(draft.content.agents, vec![detached]);
    }

    #[test]
    fn a_repeated_replacement_keeps_anchors_outside_its_ambiguity() {
        let mut draft = Draft::new("@build @plan @plan, @review");
        draft.content.agents = vec![
            agent("build", 0, 6),
            agent("plan", 7, 12),
            agent("plan", 13, 18),
            agent("review", 20, 27),
        ];
        draft.edit_text("@build @plan, @review".to_owned(), 0..0);
        assert_eq!(
            draft.content.agents,
            vec![
                agent("build", 0, 6),
                AgentMention {
                    name: "plan".to_owned(),
                    mention: None,
                },
                agent("review", 14, 21),
            ]
        );
    }

    #[test]
    fn text_edits_and_no_ops_preserve_other_owned_content_and_widget_selection() {
        let mut draft = Draft::new("@plan");
        draft.content.agents = vec![agent("plan", 0, 5)];
        draft.content.files.push(file("kept"));
        draft.content.context.push(context("kept"));
        draft.content.skills.push(SkillRef {
            id: "kept".to_owned(),
        });
        draft.content.metadata_json = Some("{\"kept\":true}".to_owned());
        let content = draft.content.clone();
        let context_buffer = draft.content.context[0].text.as_ptr();
        let metadata_buffer = draft
            .content
            .metadata_json
            .as_ref()
            .expect("metadata")
            .as_ptr();
        draft.edit_text("@plan".to_owned(), 1..3);
        assert_eq!(draft.content, content, "no-op text preserves every anchor");
        assert_eq!(draft.selection, 1..3);
        draft.edit_text("😀@plan".to_owned(), 4..9);
        assert_eq!(draft.content.files, content.files);
        assert_eq!(draft.content.context, content.context);
        assert_eq!(draft.content.skills, content.skills);
        assert_eq!(draft.content.metadata_json, content.metadata_json);
        assert_eq!(draft.content.context[0].text.as_ptr(), context_buffer);
        assert_eq!(
            draft
                .content
                .metadata_json
                .as_ref()
                .expect("metadata")
                .as_ptr(),
            metadata_buffer
        );
        assert_eq!(draft.selection, 4..9);
        assert_eq!(draft.content.agents, vec![agent("plan", 2, 7)]);
    }

    #[test]
    fn mention_offset_shifts_use_checked_arithmetic_and_u32_conversion() {
        assert_eq!(shifted_mention_offset(5, 1, 3), Some(7));
        assert_eq!(shifted_mention_offset(1, 2, 0), None);
        assert_eq!(shifted_mention_offset(u32::MAX, 0, 1), None);
        assert_eq!(shifted_mention_offset(1, 0, usize::MAX), None);
    }

    #[test]
    fn materialization_moves_full_content_and_an_existing_destination_still_wins() {
        let mut store = DraftStore::new();
        let new = DraftKey::new(
            RuntimeId::from("a"),
            "/p",
            DraftTarget::NewSession(DraftId::from("d1")),
        )
        .expect("directory is usable");
        let mut draft = Draft::default();
        draft.content.files.push(file("source"));
        draft.content.context.push(context("source"));
        store.save(new.clone(), draft.clone());
        let moved = store
            .materialize(&new, SessionId::from("s1"))
            .expect("source exists");
        assert_eq!(store.get(&moved), Some(&draft));
        assert_eq!(store.get(&new), None);
        let mut destination = Draft::default();
        destination.content.context.push(context("destination"));
        store.save(moved.clone(), destination.clone());
        store.save(new.clone(), draft);
        assert_eq!(
            store.materialize(&new, SessionId::from("s1")),
            Some(moved.clone())
        );
        assert_eq!(store.get(&moved), Some(&destination));
        assert_eq!(store.get(&new), None);
    }

    #[test]
    fn consumption_preserves_new_or_changed_files_and_context_with_the_same_text() {
        let mut captured = Draft::new("same text");
        captured.content.files = vec![file("sent"), file("changed")];
        captured.content.context = vec![context("sent"), context("changed")];
        let mut current = captured.clone();
        current.content.files[1].name = Some("new name".to_owned());
        current.content.files.push(file("new"));
        current.content.context[1].instructions = Some("new instructions".to_owned());
        current.content.context.push(context("new"));
        let newer_files = current.content.files[1..].to_vec();
        let newer_context = current.content.context[1..].to_vec();
        assert!(current.consume_captured(&captured));
        assert_eq!(current.text, "");
        assert_eq!(current.content.files, newer_files);
        assert_eq!(current.content.context, newer_context);
        assert!(current.has_sendable_content());
    }

    #[test]
    fn consumption_refuses_new_text_and_new_textual_mentions_without_mutating() {
        let captured = Draft::new("@plan");
        let mut newer = captured.clone();
        newer.text.push('!');
        let retained = newer.clone();
        assert!(!newer.consume_captured(&captured));
        assert_eq!(newer, retained);
        let mut newer = captured.clone();
        newer.content.agents.push(agent("plan", 0, 5));
        let retained = newer.clone();
        assert!(!newer.consume_captured(&captured));
        assert_eq!(newer, retained);
    }

    #[test]
    fn failed_content_merges_after_newer_typing_files_and_context() {
        let mut current = Draft::new("newer");
        current.selection = 1..3;
        current.content.files.push(file("new"));
        current.content.context.push(context("new"));
        let mut failed = Draft::new("failed");
        failed.content.files.push(file("failed"));
        failed.content.context.push(context("failed"));
        failed.content.metadata_json = Some("opaque JSON".to_owned());
        let merged = restore_failed(Some(&current), &failed).expect("independent values merge");
        assert_eq!(merged.text, "newer\n\nfailed");
        assert_eq!(merged.selection, current.selection);
        assert_eq!(merged.content.files, vec![file("new"), file("failed")]);
        assert_eq!(
            merged.content.context,
            vec![context("new"), context("failed")]
        );
        assert_eq!(merged.content.metadata_json, failed.content.metadata_json);
        assert_eq!(restore_failed(None, &failed), Ok(failed.clone()));
        assert_eq!(restore_failed(Some(&Draft::default()), &failed), Ok(failed));
    }

    #[test]
    fn equal_text_and_stable_item_identities_are_deduplicated() {
        let mut current = Draft::new("same");
        current.selection = 1..2;
        current.content.files.push(file("shared"));
        current.content.context.push(context("shared"));
        current.content.agents.push(AgentMention {
            name: "plan".to_owned(),
            mention: None,
        });
        current.content.skills.push(SkillRef {
            id: "shared-skill".to_owned(),
        });
        let mut failed = current.clone();
        failed.selection = 2..3;
        failed.content.files.push(file("shared"));
        failed.content.context.push(context("shared"));
        assert_eq!(restore_failed(Some(&current), &failed), Ok(current));
    }

    #[test]
    fn mixed_attachment_only_restore_preserves_the_current_selection_and_typing() {
        let mut current = Draft::new("newer");
        current.selection = 1..2;
        let mut failed = Draft::default();
        failed.content.files.push(file("failed"));
        let merged =
            restore_failed(Some(&current), &failed).expect("attachment-only failure merges");
        assert_eq!(merged.text, current.text);
        assert_eq!(merged.selection, current.selection);
        let merged =
            restore_failed(Some(&failed), &current).expect("attachment draft gains failed text");
        assert_eq!(merged.text, current.text);
        assert_eq!(merged.selection, failed.selection);
        assert_eq!(merged.content.files, failed.content.files);
    }

    #[test]
    fn conflicting_metadata_or_stable_payloads_refuse_without_mutating_inputs() {
        let mut current = Draft::new("newer");
        current.content.metadata_json = Some("{\"value\":1}".to_owned());
        let mut failed = Draft::new("failed");
        failed.content.metadata_json = Some("{\"value\":2}".to_owned());
        let original_current = current.clone();
        let original_failed = failed.clone();
        assert_eq!(
            restore_failed(Some(&current), &failed),
            Err(ContentConflict::Metadata)
        );
        assert_eq!(current, original_current);
        assert_eq!(failed, original_failed);
        current.content.metadata_json = None;
        failed.content.metadata_json = None;
        current.content.files.push(file("shared"));
        failed.content.files.push(file("shared"));
        failed.content.files[0].content = DraftFileContent::Reference {
            uri: "file:///new".to_owned(),
        };
        assert_eq!(
            restore_failed(Some(&current), &failed),
            Err(ContentConflict::File {
                id: AttachmentId::from("shared"),
            })
        );
        current.content.files.clear();
        failed.content.files.clear();
        current.content.context.push(context("shared"));
        failed.content.context.push(context("shared"));
        failed.content.context[0].instructions = Some("changed".to_owned());
        assert_eq!(
            restore_failed(Some(&current), &failed),
            Err(ContentConflict::Context {
                id: ContextItemId::from("shared"),
            })
        );
    }

    #[test]
    fn failed_mentions_rebase_by_checked_utf16_units_including_non_bmp_text() {
        let mut current = Draft::new("😀 @build");
        current.selection = 4..4;
        current.content.agents.push(agent("build", 3, 9));
        let mut failed = Draft::new("@plan");
        failed.content.agents.push(agent("plan", 0, 5));
        let merged = restore_failed(Some(&current), &failed).expect("mention offsets fit");
        assert_eq!(merged.text, "😀 @build\n\n@plan");
        assert_eq!(merged.selection, current.selection);
        assert_eq!(
            merged.content.agents,
            vec![agent("build", 3, 9), agent("plan", 11, 16)]
        );
        failed.content.agents[0]
            .mention
            .as_mut()
            .expect("typed mention")
            .end = u32::MAX;
        let retained = failed.clone();
        assert_eq!(
            restore_failed(Some(&current), &failed),
            Err(ContentConflict::MentionOffsetOverflow)
        );
        assert_eq!(
            failed, retained,
            "overflow must retain the full failed capture"
        );
    }

    #[test]
    fn draft_diagnostics_hide_text() {
        assert!(!format!("{:?}", Draft::new("private-prompt")).contains("private-prompt"));
    }

    #[test]
    fn materializing_keeps_a_newer_session_draft() {
        let mut store = DraftStore::new();
        let new = DraftKey::new(
            RuntimeId::from("a"),
            "/p",
            DraftTarget::NewSession(DraftId::from("d1")),
        )
        .expect("directory is usable");
        store.save(new.clone(), Draft::new("old"));
        store.save(key("a", "/p", "s1"), Draft::new("newer"));
        let moved = store
            .materialize(&new, SessionId::from("s1"))
            .expect("draft existed");
        assert_eq!(
            store.get(&moved).map(|draft| draft.text.as_str()),
            Some("newer")
        );
        assert_eq!(store.get(&new), None);
    }
}
