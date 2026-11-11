//! One editable textarea over native content and immutable rich queue sources.
//!
//! The toolkit owns text editing and undo. This owner retains its committed rope
//! and small source layouts keyed by toolkit history checkpoints. Only immutable
//! editing snapshots enter the private spool; the journal retains compact refs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::input::{CommittedInputEdit, InputReplacement, Rope, RopeExt};
use gpui_kit::{App, AsyncWindowContext, Context, Task, WeakEntity, Window};
use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::ids::{AttachmentId, RuntimeId, SessionId};
use openchamber_native_transport::queue_manifest::{
    QueueLocalCapture, QueueLocalItem, QueueMention, QueuePayloadItem,
};
use serde::{Deserialize, Serialize};

use crate::attachment_spool::{BlobDescriptor, BlobId};
use crate::draft_companion::{self, CompanionCapture};
use crate::mutation_journal::{MAX_ENTRIES, Owner};
use crate::queue_capture::{
    QUEUE_CHUNK_BYTES, QueueNativeSource, QueueObjectId, QueueObjectKind, QueueOperationId,
    QueueSpoolRef, sha256,
};
use crate::queue_payload_io::{self, QueueCaptureFailure};
use crate::queue_storage::{ComposerConsumption, ComposerEditRef, DraftCompanionRef, QueueObject};
use crate::queue_take::{self, MaterializedQueueItem};
use crate::workspace::Workspace;

const SAVE_DELAY: Duration = Duration::from_millis(500);
const HISTORY_LAYOUTS: usize = 1001;

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ComposerSourceKey {
    Native,
    Companion {
        operation: QueueOperationId,
        companion: QueueObjectId,
        item: usize,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Anchor {
    start: u64,
    end: u64,
    #[serde(with = "anchor_text")]
    text: Arc<str>,
}

mod anchor_text {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::sync::Arc;

    pub(super) fn serialize<S: Serializer>(
        text: &Arc<str>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(text)
    }
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Arc<str>, D::Error> {
        String::deserialize(deserializer).map(Arc::from)
    }
}

impl Anchor {
    fn capture(mention: &QueueMention) -> Self {
        Self {
            start: mention.start,
            end: mention.end,
            text: Arc::from(mention.text.as_str()),
        }
    }
    fn restore(&self) -> QueueMention {
        QueueMention {
            start: self.start,
            end: self.end,
            text: self.text.to_string(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceState {
    key: ComposerSourceKey,
    /// Shared across undo layouts; rebuilt from verified source metadata at load.
    #[serde(skip)]
    attachments: Arc<[ComposerAttachment]>,
    edited: bool,
    /// Captured delivered offsets cannot be mapped into different display text.
    positions_in_display: bool,
    contexts: usize,
    files: Vec<Option<Anchor>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    file_references: Vec<Option<Anchor>>,
    agents: Vec<Option<Anchor>>,
    skills: Vec<Option<Anchor>>,
    removed_files: Vec<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    removed_file_references: Vec<usize>,
    removed_agents: Vec<usize>,
    removed_skills: Vec<usize>,
}

impl SourceState {
    fn has_removals(&self) -> bool {
        !self.removed_files.is_empty()
            || !self.removed_file_references.is_empty()
            || !self.removed_agents.is_empty()
            || !self.removed_skills.is_empty()
    }

    fn has_sendable_references(&self) -> bool {
        self.contexts > 0
            || self.removed_files.len() < self.files.len()
            || self.removed_file_references.len() < self.file_references.len()
    }

    fn native(draft: &Draft) -> Self {
        Self {
            key: ComposerSourceKey::Native,
            attachments: Arc::default(),
            edited: false,
            positions_in_display: true,
            contexts: draft.content.context.len(),
            files: Vec::new(),
            file_references: Vec::new(),
            skills: vec![None; draft.content.skills.len()],
            agents: draft
                .content
                .agents
                .iter()
                .map(|agent| {
                    agent.mention.as_ref().map(|mention| Anchor {
                        start: u64::from(mention.start),
                        end: u64::from(mention.end),
                        text: Arc::from(mention.text.as_str()),
                    })
                })
                .collect(),
            removed_files: Vec::new(),
            removed_file_references: Vec::new(),
            removed_agents: Vec::new(),
            removed_skills: Vec::new(),
        }
    }

    fn local(key: ComposerSourceKey, item: &QueueLocalItem) -> Self {
        let mut attachments =
            Vec::with_capacity(item.files.len() + item.prompt_parts.file_references.len());
        for (ordinal, file) in item.files.iter().enumerate() {
            attachments.push(ComposerAttachment {
                name: file.name.clone(),
                kind: ComposerReferenceKind::File,
                ordinal,
            });
        }
        for (ordinal, file) in item.prompt_parts.file_references.iter().enumerate() {
            attachments.push(ComposerAttachment {
                name: file.name.clone(),
                kind: ComposerReferenceKind::FileReference,
                ordinal,
            });
        }
        Self {
            key,
            attachments: attachments.into(),
            edited: false,
            positions_in_display: item.content == item.text,
            contexts: item.prompt_parts.context.len(),
            files: item
                .files
                .iter()
                .map(|file| file.mention.as_ref().map(Anchor::capture))
                .collect(),
            file_references: item
                .prompt_parts
                .file_references
                .iter()
                .map(|file| file.mention.as_ref().map(Anchor::capture))
                .collect(),
            agents: item
                .prompt_parts
                .agents
                .iter()
                .map(|agent| agent.mention.as_ref().map(Anchor::capture))
                .collect(),
            skills: item
                .prompt_parts
                .skills
                .iter()
                .map(|skill| skill.mention.as_ref().map(Anchor::capture))
                .collect(),
            removed_files: Vec::new(),
            removed_file_references: Vec::new(),
            removed_agents: Vec::new(),
            removed_skills: Vec::new(),
        }
    }

    fn edit(&mut self, old: &str, range: Range<usize>, inserted: &str) {
        self.edited = true;
        let start = old[..range.start].encode_utf16().count() as u64;
        let end = start + old[range].encode_utf16().count() as u64;
        let inserted = inserted.encode_utf16().count() as u64;
        let mut requested: HashSet<_> = self
            .files
            .iter()
            .chain(&self.file_references)
            .chain(&self.agents)
            .chain(&self.skills)
            .filter_map(Option::as_ref)
            .flat_map(|anchor| [anchor.start, anchor.end])
            .collect();
        let mut boundaries = HashMap::new();
        let mut units = 0;
        if requested.remove(&0) {
            boundaries.insert(0, 0);
        }
        for (byte, character) in old.char_indices() {
            units += character.len_utf16() as u64;
            if requested.remove(&units) {
                boundaries.insert(units, byte + character.len_utf8());
            }
            if requested.is_empty() {
                break;
            }
        }
        for mention in self
            .files
            .iter_mut()
            .chain(&mut self.file_references)
            .chain(&mut self.agents)
            .chain(&mut self.skills)
        {
            let Some(anchor) = mention else {
                continue;
            };
            let valid = boundaries
                .get(&anchor.start)
                .zip(boundaries.get(&anchor.end))
                .is_some_and(|(start, end)| old.get(*start..*end) == Some(anchor.text.as_ref()));
            if !self.positions_in_display || anchor.start > anchor.end || !valid {
                *mention = None;
            } else if anchor.end <= start {
                // An insertion at the end of a mention leaves its anchor alone.
            } else if anchor.start >= end {
                let shifted = anchor
                    .start
                    .checked_sub(end - start)
                    .and_then(|offset| offset.checked_add(inserted))
                    .zip(
                        anchor
                            .end
                            .checked_sub(end - start)
                            .and_then(|offset| offset.checked_add(inserted)),
                    );
                match shifted {
                    Some((start, end)) if end <= 9_007_199_254_740_991 => {
                        anchor.start = start;
                        anchor.end = end;
                    }
                    _ => *mention = None,
                }
            } else {
                *mention = None;
            }
        }
        self.positions_in_display = true;
    }

    fn project(&self, item: &mut QueueLocalItem) -> Result<(), QueueCaptureFailure> {
        if self.files.len() != item.files.len()
            || self.file_references.len() != item.prompt_parts.file_references.len()
            || self.agents.len() != item.prompt_parts.agents.len()
            || self.skills.len() != item.prompt_parts.skills.len()
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        for (file, anchor) in item.files.iter_mut().zip(&self.files) {
            file.mention = anchor.as_ref().map(Anchor::restore);
        }
        for (file, anchor) in item
            .prompt_parts
            .file_references
            .iter_mut()
            .zip(&self.file_references)
        {
            file.mention = anchor.as_ref().map(Anchor::restore);
        }
        for (agent, anchor) in item.prompt_parts.agents.iter_mut().zip(&self.agents) {
            agent.mention = anchor.as_ref().map(Anchor::restore);
        }
        for (skill, anchor) in item.prompt_parts.skills.iter_mut().zip(&self.skills) {
            skill.mention = anchor.as_ref().map(Anchor::restore);
        }
        retain_ordinals(&mut item.files, &self.removed_files);
        retain_ordinals(
            &mut item.prompt_parts.file_references,
            &self.removed_file_references,
        );
        retain_ordinals(&mut item.prompt_parts.agents, &self.removed_agents);
        retain_ordinals(&mut item.prompt_parts.skills, &self.removed_skills);
        Ok(())
    }

    fn project_display(
        &self,
        item: &mut QueueLocalItem,
        display: &str,
        present: bool,
    ) -> Result<bool, QueueCaptureFailure> {
        if !self.edited && present && display != item.content {
            return Err(QueueCaptureFailure::Invalid);
        }
        self.project(item)?;
        let reprocess_text = self.edited || !present;
        item.content = display.to_owned();
        if reprocess_text {
            item.text = String::new();
        }
        if !present {
            for file in &mut item.files {
                file.mention = None;
            }
            for file in &mut item.prompt_parts.file_references {
                file.mention = None;
            }
            for agent in &mut item.prompt_parts.agents {
                agent.mention = None;
            }
            for skill in &mut item.prompt_parts.skills {
                skill.mention = None;
            }
        }
        Ok(reprocess_text)
    }

    fn exclusions(&self, source: usize, item: &QueueLocalItem) -> ComposerSourceExclusions {
        let occurrence =
            |ordinal,
             identity: String,
             original: &Option<QueueMention>,
             display: &Option<Anchor>| ComposerExcludedReference {
                ordinal,
                identity,
                original_mention: original.clone(),
                display_mention: display.as_ref().map(Anchor::restore),
            };
        ComposerSourceExclusions {
            source,
            agents: self
                .removed_agents
                .iter()
                .filter_map(|ordinal| {
                    item.prompt_parts.agents.get(*ordinal).map(|agent| {
                        occurrence(
                            *ordinal,
                            agent.name.clone(),
                            &agent.mention,
                            &self.agents[*ordinal],
                        )
                    })
                })
                .collect(),
            skills: self
                .removed_skills
                .iter()
                .filter_map(|ordinal| {
                    item.prompt_parts.skills.get(*ordinal).map(|skill| {
                        occurrence(
                            *ordinal,
                            skill.id.clone(),
                            &skill.mention,
                            &self.skills[*ordinal],
                        )
                    })
                })
                .collect(),
            files: self
                .removed_files
                .iter()
                .filter_map(|ordinal| {
                    item.files.get(*ordinal).map(|file| {
                        occurrence(
                            *ordinal,
                            file.name.clone(),
                            &file.mention,
                            &self.files[*ordinal],
                        )
                    })
                })
                .collect(),
            file_references: self
                .removed_file_references
                .iter()
                .filter_map(|ordinal| {
                    item.prompt_parts.file_references.get(*ordinal).map(|file| {
                        occurrence(
                            *ordinal,
                            file.uri.clone(),
                            &file.mention,
                            &self.file_references[*ordinal],
                        )
                    })
                })
                .collect(),
        }
    }
}

fn retain_ordinals<T>(values: &mut Vec<T>, removed: &[usize]) {
    let mut ordinal = 0;
    values.retain(|_| {
        let keep = !removed.contains(&ordinal);
        ordinal += 1;
        keep
    });
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum SpanKind {
    Source(ComposerSourceKey),
    Literal { edited: bool },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Span {
    kind: SpanKind,
    bytes: usize,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Layout {
    spans: Vec<Span>,
    sources: Vec<SourceState>,
}

impl Layout {
    /// Reference removals and unchanged empty-display sources are independent of
    /// text undo, including checkpoints preceding a semantic-only append.
    fn retain_semantic_state(&mut self, current: &Self) {
        for source in &current.sources {
            if let Some(restored) = self
                .sources
                .iter_mut()
                .find(|restored| restored.key == source.key)
            {
                restored.removed_files = source.removed_files.clone();
                restored.removed_file_references = source.removed_file_references.clone();
                restored.removed_agents = source.removed_agents.clone();
                restored.removed_skills = source.removed_skills.clone();
            } else if !matches!(&source.key, ComposerSourceKey::Native) {
                self.sources.push(source.clone());
            }
        }
        for span in &current.spans {
            let SpanKind::Source(key) = &span.kind else {
                continue;
            };
            if span.bytes == 0
                && !matches!(key, ComposerSourceKey::Native)
                && current
                    .sources
                    .iter()
                    .any(|source| &source.key == key && !source.edited)
                && !self.contains_source(key)
            {
                self.spans.push(span.clone());
            }
        }
    }

    fn contains_source(&self, key: &ComposerSourceKey) -> bool {
        self.spans
            .iter()
            .any(|span| matches!(&span.kind, SpanKind::Source(source) if source == key))
    }

    fn native(draft: &Draft) -> Self {
        Self {
            spans: vec![Span {
                kind: SpanKind::Source(ComposerSourceKey::Native),
                bytes: draft.text.len(),
            }],
            sources: vec![SourceState::native(draft)],
        }
    }

    fn display(&self, text: &Rope, key: &ComposerSourceKey) -> String {
        let mut offset = 0;
        for span in &self.spans {
            if matches!(&span.kind, SpanKind::Source(source) if source == key) {
                return text.slice(offset..offset + span.bytes).to_string();
            }
            offset += span.bytes;
        }
        String::new()
    }

    fn replace(
        &mut self,
        text: &mut Rope,
        edit: &InputReplacement,
    ) -> Result<(), QueueCaptureFailure> {
        if edit.range.start > edit.range.end
            || edit.range.end > text.len()
            || self.spans.iter().map(|span| span.bytes).sum::<usize>() != text.len()
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        let mut offset = 0;
        let target = self
            .spans
            .iter()
            .position(|span| {
                let end = offset + span.bytes;
                let contains = edit.range.start < end;
                offset = end;
                contains
            })
            .unwrap_or(self.spans.len() - 1);
        offset = 0;
        for (ordinal, span) in self.spans.iter_mut().enumerate() {
            let end = offset + span.bytes;
            let start_in_span = edit.range.start.saturating_sub(offset).min(span.bytes);
            let end_in_span = edit.range.end.saturating_sub(offset).min(span.bytes);
            let inserted = if ordinal == target {
                edit.text.as_ref()
            } else {
                ""
            };
            if start_in_span != end_in_span || !inserted.is_empty() {
                match &mut span.kind {
                    SpanKind::Source(key) => {
                        let source = self
                            .sources
                            .iter_mut()
                            .find(|source| &source.key == key)
                            .ok_or(QueueCaptureFailure::Invalid)?;
                        let old = text.slice(offset..end).to_string();
                        source.edit(&old, start_in_span..end_in_span, inserted);
                    }
                    SpanKind::Literal { edited } => *edited = true,
                }
                span.bytes = span.bytes - (end_in_span - start_in_span) + inserted.len();
            }
            offset = end;
        }
        text.replace(edit.range.clone(), edit.text.as_ref());
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditDocument {
    version: u32,
    owner: Owner,
    generation: u64,
    revision: u64,
    companions: Vec<DraftCompanionRef>,
    shown: Vec<DraftCompanionRef>,
    text: String,
    start: usize,
    end: usize,
    layout: Layout,
}

#[cfg(test)]
pub(crate) fn literal_submission_edit_for_test(
    owner: &DraftKey,
    companions: &[DraftCompanionRef],
    generation: u64,
    revision: u64,
) -> Vec<u8> {
    let text = "exact captured edit λ".to_owned();
    let document = EditDocument {
        version: 1,
        owner: Owner::capture(owner),
        generation,
        revision,
        companions: companions.to_vec(),
        shown: companions.to_vec(),
        start: text.len(),
        end: text.len(),
        layout: Layout {
            spans: vec![Span {
                kind: SpanKind::Literal { edited: true },
                bytes: text.len(),
            }],
            sources: Vec::new(),
        },
        text,
    };
    serde_json::to_vec(&document).expect("bounded genuine editing snapshot")
}

impl EditDocument {
    fn validate(
        &self,
        owner: &DraftKey,
        reference: &ComposerEditRef,
    ) -> Result<(), QueueCaptureFailure> {
        if self.version != 1
            || self.owner.clone().into_key()?.ne(owner)
            || self.generation != reference.generation
            || self.revision != reference.revision
            || self.companions != reference.companions
            || self.start > self.end
            || !self.text.is_char_boundary(self.start)
            || !self.text.is_char_boundary(self.end)
            || self.layout.spans.is_empty()
            || self.layout.spans.len() > MAX_ENTRIES
            || self.layout.sources.len() > MAX_ENTRIES
            || self.shown.len() > self.companions.len()
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        let mut next = 0;
        for reference in &self.companions {
            if self.shown.get(next) == Some(reference) {
                next += 1;
            }
        }
        if next != self.shown.len() {
            return Err(QueueCaptureFailure::Invalid);
        }
        let mut offset = 0usize;
        let mut sources = HashSet::new();
        for source in &self.layout.sources {
            if !sources.insert(source.key.clone()) {
                return Err(QueueCaptureFailure::Invalid);
            }
            if source
                .files
                .len()
                .saturating_add(source.file_references.len())
                .saturating_add(source.agents.len())
                .saturating_add(source.skills.len())
                > MAX_ENTRIES
                || source.contexts > MAX_ENTRIES
            {
                return Err(QueueCaptureFailure::Invalid);
            }
            if let ComposerSourceKey::Companion {
                operation,
                companion,
                item,
            } = &source.key
                && (*item >= 20
                    || !self.companions.iter().any(|reference| {
                        &reference.operation == operation && &reference.companion == companion
                    }))
            {
                return Err(QueueCaptureFailure::Invalid);
            }
            for (removed, count) in [
                (&source.removed_files, source.files.len()),
                (
                    &source.removed_file_references,
                    source.file_references.len(),
                ),
                (&source.removed_agents, source.agents.len()),
                (&source.removed_skills, source.skills.len()),
            ] {
                if removed.iter().any(|ordinal| *ordinal >= count)
                    || removed.iter().collect::<HashSet<_>>().len() != removed.len()
                {
                    return Err(QueueCaptureFailure::Invalid);
                }
            }
        }
        let mut spans = HashSet::new();
        for span in &self.layout.spans {
            offset = offset
                .checked_add(span.bytes)
                .ok_or(QueueCaptureFailure::Invalid)?;
            if !self.text.is_char_boundary(offset) {
                return Err(QueueCaptureFailure::Invalid);
            }
            if let SpanKind::Source(key) = &span.kind
                && (!sources.contains(key) || !spans.insert(key))
            {
                return Err(QueueCaptureFailure::Invalid);
            }
        }
        if offset != self.text.len() {
            return Err(QueueCaptureFailure::Invalid);
        }
        Ok(())
    }
}

struct PendingAppend {
    before: Layout,
    after: Layout,
    suffix: String,
}

enum ReadState {
    Ready,
    Loading,
    Failed,
}

struct Document {
    generation: u64,
    revision: u64,
    text: Rope,
    selection: Range<usize>,
    layout: Layout,
    known: Vec<DraftCompanionRef>,
    durable: Option<ComposerEditRef>,
    read: ReadState,
    history: HashMap<u64, Layout>,
    history_order: VecDeque<u64>,
    append: Option<PendingAppend>,
    deferred: Option<Loaded>,
    planned: Option<ComposerEditRef>,
    save_failure: Option<ComposerSaveFailure>,
}

impl Document {
    fn stage_loaded(&mut self, loaded: Loaded) -> Option<Range<usize>> {
        let before = self.layout.clone();
        let mut after = before.clone();
        let mut suffix = String::new();
        let mut restored_selection = None;
        let restored_sources = loaded
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.shown.clone())
            .unwrap_or_default();
        if let Some(snapshot) = loaded.snapshot {
            if self.text.len() > 0 {
                for span in &mut after.spans {
                    span.kind = SpanKind::Literal { edited: true };
                }
                after.sources.clear();
                if !snapshot.text.is_empty() {
                    suffix.push_str("\n\n");
                    after.spans.push(Span {
                        kind: SpanKind::Literal { edited: false },
                        bytes: 2,
                    });
                }
            } else {
                after.spans.clear();
                after.sources.clear();
            }
            suffix.push_str(&snapshot.text);
            after.spans.extend(snapshot.layout.spans);
            after.sources.extend(snapshot.layout.sources);
            if self.text.len() == 0 {
                restored_selection = Some(snapshot.start..snapshot.end);
            }
        }
        for (key, content, state) in loaded.headers {
            if let Some(source) = after.sources.iter_mut().find(|source| source.key == key) {
                source.attachments = state.attachments;
                continue;
            }
            // A source absent from a saved layout was explicitly text-deleted.
            if matches!(&key, ComposerSourceKey::Companion { operation, companion, .. }
                if restored_sources.iter().any(|reference| &reference.operation == operation && &reference.companion == companion))
            {
                after.sources.push(state);
                continue;
            }
            if self.text.len() + suffix.len() > 0 && !content.is_empty() {
                suffix.push_str("\n\n");
                after.spans.push(Span {
                    kind: SpanKind::Literal { edited: false },
                    bytes: 2,
                });
            }
            after.spans.push(Span {
                kind: SpanKind::Source(key),
                bytes: content.len(),
            });
            after.sources.push(state);
            suffix.push_str(&content);
        }
        self.known = loaded.references;
        self.read = ReadState::Ready;
        if suffix.is_empty() {
            self.layout = after;
            self.revision = self.revision.saturating_add(1);
            self.clear_unwritten_save_failure();
        } else {
            self.append = Some(PendingAppend {
                before,
                after,
                suffix,
            });
        }
        restored_selection
    }

    fn save_failed(&mut self, mut capture: ComposerCapture, error: QueueCaptureFailure) {
        if capture.generation != self.generation
            || self.durable.as_ref().is_some_and(|durable| {
                durable.generation == capture.generation && durable.revision >= capture.revision
            })
        {
            return;
        }
        capture.close = None;
        capture.explicit_retry = false;
        if capture.planned.is_none() {
            capture.planned = self.planned.clone().filter(|planned| {
                (planned.generation, planned.revision) == (capture.generation, capture.revision)
            });
        }
        self.save_failure = Some(ComposerSaveFailure { capture, error });
        self.clear_unwritten_save_failure();
    }

    fn clear_unwritten_save_failure(&mut self) {
        if self.save_failure.as_ref().is_some_and(|failure| {
            failure.capture.planned.is_none()
                && failure.capture.generation == self.generation
                && failure.capture.revision < self.revision
        }) {
            self.save_failure = None;
        }
    }

    fn select(&mut self, selection: Range<usize>) {
        if self.selection != selection {
            self.selection = selection;
            self.revision = self.revision.saturating_add(1);
            self.clear_unwritten_save_failure();
        }
    }

    fn ready_for_capture(&self, references: &[DraftCompanionRef]) -> bool {
        self.generation > 0
            && self.generation < u64::MAX
            && self.revision > 0
            && self.revision < u64::MAX
            && matches!(self.read, ReadState::Ready)
            && self.deferred.is_none()
            && self.append.is_none()
            && self.known == references
            && self.save_failure.is_none()
    }

    fn dirty(&self) -> bool {
        self.durable.as_ref().is_none_or(|saved| {
            (saved.generation, saved.revision) != (self.generation, self.revision)
        })
    }

    fn remember(&mut self, checkpoint: u64, layout: Layout) {
        if self.history.insert(checkpoint, layout).is_none() {
            self.history_order.push_back(checkpoint);
        }
        while self.history_order.len() > HISTORY_LAYOUTS {
            if let Some(old) = self.history_order.pop_front() {
                self.history.remove(&old);
            }
        }
    }

    fn changed(&mut self, event: &CommittedInputEdit) -> Result<(), QueueCaptureFailure> {
        let before = self.layout.clone();
        if let Some(append) = self.append.take() {
            if event.replacements.len() != 1
                || event.replacements[0].range != (self.text.len()..self.text.len())
                || event.replacements[0].text.as_ref() != append.suffix
            {
                return Err(QueueCaptureFailure::Invalid);
            }
            self.remember(event.before, append.before);
            self.layout = append.after;
        } else if event.before != event.after && self.history.contains_key(&event.after) {
            self.layout = self.history[&event.after].clone();
            self.layout.retain_semantic_state(&before);
        } else {
            self.remember(event.before, before);
            let mut text = self.text.clone();
            let mut layout = self.layout.clone();
            for edit in &event.replacements {
                layout.replace(&mut text, edit)?;
            }
            if text != event.text {
                return Err(QueueCaptureFailure::Invalid);
            }
            self.layout = layout;
        }
        self.text = event.text.clone();
        self.selection = event.selection.clone();
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(QueueCaptureFailure::Invalid)?;
        self.clear_unwritten_save_failure();
        self.remember(event.after, self.layout.clone());
        Ok(())
    }
}

/// Window-local state only. The journal owns every durable reference.
#[derive(Default)]
pub(crate) struct ComposerDocuments {
    documents: HashMap<DraftKey, Document>,
    generations: HashMap<DraftKey, u64>,
    native_revisions: HashMap<DraftKey, u64>,
    native_authority: Arc<()>,
    loads: HashMap<DraftKey, Task<()>>,
    saves: HashMap<DraftKey, Task<()>>,
    epoch: u64,
    frozen: Option<u64>,
    submissions: HashMap<QueueOperationId, submission::ComposerSubmission>,
    submission_retries: HashMap<QueueOperationId, Task<()>>,
}

impl ComposerDocuments {
    pub(crate) fn contains(&self, owner: &DraftKey) -> bool {
        self.documents.contains_key(owner)
    }

    /// Forgets every in-flight submission, for tests that discard an actor by hand.
    #[cfg(test)]
    pub(crate) fn clear_submissions_for_test(&mut self) {
        self.submissions.clear();
    }

    /// Confirmed deletion invalidates only these session owners. Accepted worker
    /// writes still drain, but their old generations cannot restore editor state.
    pub(crate) fn forget_deleted_sessions(
        &mut self,
        runtime: &RuntimeId,
        sessions: &[SessionId],
    ) -> Vec<AttachmentId> {
        let sessions: HashSet<_> = sessions.iter().collect();
        let owners: HashSet<_> = self.documents.keys().chain(self.generations.keys()).chain(self.native_revisions.keys())
            .chain(self.loads.keys()).chain(self.saves.keys())
            .filter(|owner| owner.runtime() == runtime && matches!(owner.target(), DraftTarget::Session(session) if sessions.contains(session)))
            .cloned().collect();
        let mut roots = HashSet::new();
        roots.extend(self.forget_submission_sessions(runtime, &sessions));
        for owner in owners {
            let document = self.documents.remove(&owner);
            let generation = self
                .generations
                .get(&owner)
                .copied()
                .unwrap_or(1)
                .max(document.as_ref().map_or(1, |document| document.generation));
            if let Some(failure) = document.and_then(|document| document.save_failure) {
                for file in failure.capture.composer.native.draft.content.files {
                    roots.insert(file.id);
                    if let Some(group) = file.source_document_id {
                        roots.insert(group);
                    }
                }
            }
            self.generations
                .insert(owner.clone(), generation.saturating_add(1));
            self.native_revisions.remove(&owner);
            self.loads.remove(&owner);
            self.saves.remove(&owner);
        }
        let mut roots: Vec<_> = roots.into_iter().collect();
        roots.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        roots
    }
}

#[path = "composer_submission.rs"]
mod submission;
pub(crate) use submission::{
    SubmissionClosePermit, complete_submission, drain_submissions, ensure_submission_edit,
};

/// Immutable submission intent. Flush seals its exact editing state before a
/// Send/Prefix operation may consume the corresponding composer revision.
#[derive(Clone)]
pub(crate) struct ComposerCapture {
    pub(crate) composer: CompanionCapture,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) edit: Option<ComposerEditRef>,
    snapshot: Option<(Rope, Range<usize>, Layout, Vec<DraftCompanionRef>)>,
    planned: Option<ComposerEditRef>,
    close: Option<ComposerClosePermit>,
    explicit_retry: bool,
    /// This window's live capture capability never crosses a persisted boundary.
    native_authority: Option<Arc<()>>,
}

impl ComposerCapture {
    /// Reconstruct only from the caller's verified published source capture.
    pub(crate) fn from_published_at(
        composer: CompanionCapture,
        edit: Option<ComposerEditRef>,
        generation: u64,
        revision: u64,
    ) -> Result<Self, QueueCaptureFailure> {
        if edit
            .as_ref()
            .is_some_and(|edit| edit.companions != composer.references)
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        if generation == 0
            || revision == 0
            || edit
                .as_ref()
                .is_some_and(|edit| (edit.generation, edit.revision) != (generation, revision))
        {
            return Err(QueueCaptureFailure::Invalid);
        }
        Ok(Self {
            composer,
            generation,
            revision,
            edit,
            snapshot: None,
            planned: None,
            close: None,
            explicit_retry: false,
            native_authority: None,
        })
    }
    /// Only an explicit storage Retry action may authorize repairing a partial put.
    pub(crate) fn for_explicit_retry(mut self) -> Self {
        self.explicit_retry = true;
        self
    }
}

struct ComposerSaveFailure {
    capture: ComposerCapture,
    error: QueueCaptureFailure,
}

/// Constructed only by the document owner's freeze transition. It permits no
/// HTTP request, source publication, retirement or unrelated journal mutation.
#[derive(Clone)]
pub(crate) struct ComposerClosePermit {
    epoch: u64,
    owner: DraftKey,
    generation: u64,
    revision: u64,
    reference: Option<ComposerEditRef>,
}

impl ComposerClosePermit {
    fn matches(&self, owner: &DraftKey, edit: &ComposerEditRef, epoch: Option<u64>) -> bool {
        epoch == Some(self.epoch)
            && &self.owner == owner
            && self.generation == edit.generation
            && self.revision == edit.revision
            && self.reference.as_ref() == Some(edit)
    }
}

pub(crate) enum ComposerCompanionInput {
    Original(Box<MaterializedQueueItem>),
    /// Reference-only projections retain the captured delivered text. Text edits
    /// require the normal captured-owner mention/snippet reprocessor before send.
    Edited {
        capture: Box<QueueLocalCapture>,
        files: Vec<BlobDescriptor>,
        reprocess_text: bool,
    },
}

pub(crate) enum ComposerTextPiece {
    Source(usize),
    Literal { text: String, edited: bool },
}

pub(crate) struct ComposerInputs {
    /// Effective native constituent only. Never put this expanded result in a journal.
    pub(crate) native: QueueNativeSource,
    pub(crate) companions: Vec<ComposerCompanionInput>,
    /// Source 0 is native; remaining sources follow companion/context order.
    pub(crate) text: Vec<ComposerTextPiece>,
    pub(crate) exclusions: Vec<ComposerSourceExclusions>,
}

/// Suppress newly parsed creation for this source only. The exact original
/// occurrence has already been filtered; retained equal-ID occurrences survive.
pub(crate) struct ComposerSourceExclusions {
    pub(crate) source: usize,
    pub(crate) agents: Vec<ComposerExcludedReference>,
    pub(crate) skills: Vec<ComposerExcludedReference>,
    pub(crate) files: Vec<ComposerExcludedReference>,
    pub(crate) file_references: Vec<ComposerExcludedReference>,
}

pub(crate) struct ComposerExcludedReference {
    pub(crate) ordinal: usize,
    pub(crate) identity: String,
    pub(crate) original_mention: Option<QueueMention>,
    pub(crate) display_mention: Option<QueueMention>,
}

#[derive(Clone, Copy)]
pub(crate) enum ComposerReferenceKind {
    File,
    FileReference,
    #[cfg(test)]
    Agent,
    #[cfg(test)]
    Skill,
}

/// Display metadata from an already verified source, never file-read authority.
pub(crate) struct ComposerAttachment {
    pub(crate) name: String,
    pub(crate) kind: ComposerReferenceKind,
    pub(crate) ordinal: usize,
}

struct Loaded {
    snapshot: Option<EditDocument>,
    headers: Vec<(ComposerSourceKey, String, SourceState)>,
    references: Vec<DraftCompanionRef>,
}

impl Workspace {
    pub(crate) fn composer_companion_attachments(
        &self,
        owner: &DraftKey,
    ) -> impl Iterator<Item = (&ComposerSourceKey, &ComposerAttachment)> {
        self.composer_documents
            .documents
            .get(owner)
            .into_iter()
            .flat_map(|document| &document.layout.sources)
            .flat_map(|source| {
                source
                    .attachments
                    .iter()
                    .filter(move |attachment| {
                        let removed = match attachment.kind {
                            ComposerReferenceKind::File => &source.removed_files,
                            ComposerReferenceKind::FileReference => &source.removed_file_references,
                            #[cfg(test)]
                            ComposerReferenceKind::Agent | ComposerReferenceKind::Skill => {
                                return false;
                            }
                        };
                        !removed.contains(&attachment.ordinal)
                    })
                    .map(move |attachment| (&source.key, attachment))
            })
    }

    /// Existing rich-reference controls address an exact occurrence. Removing a
    /// reference does not rewrite an immutable original or guess a text match.
    pub(crate) fn remove_composer_reference(
        &mut self,
        owner: &DraftKey,
        key: &ComposerSourceKey,
        kind: ComposerReferenceKind,
        ordinal: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), QueueCaptureFailure> {
        if self.close_in_progress()
            || self.composer_documents.frozen.is_some()
            || matches!(key, ComposerSourceKey::Native)
        {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let document = self
            .composer_documents
            .documents
            .get_mut(owner)
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let source = document
            .layout
            .sources
            .iter_mut()
            .find(|source| &source.key == key)
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let (removed, count) = match kind {
            ComposerReferenceKind::File => (&mut source.removed_files, source.files.len()),
            ComposerReferenceKind::FileReference => (
                &mut source.removed_file_references,
                source.file_references.len(),
            ),
            #[cfg(test)]
            ComposerReferenceKind::Agent => (&mut source.removed_agents, source.agents.len()),
            #[cfg(test)]
            ComposerReferenceKind::Skill => (&mut source.removed_skills, source.skills.len()),
        };
        if ordinal >= count {
            return Err(QueueCaptureFailure::Invalid);
        }
        if removed.contains(&ordinal) {
            return Ok(());
        }
        removed.push(ordinal);
        document.revision = document.revision.saturating_add(1);
        document.clear_unwritten_save_failure();
        self.schedule_composer_document_save(owner, window, cx);
        cx.notify();
        Ok(())
    }
    pub(crate) fn compound_composer_sendable(&self) -> Option<bool> {
        let owner = self.composer_key.as_ref()?;
        let document = self.composer_documents.documents.get(owner)?;
        Some(
            document
                .text
                .chars()
                .any(|character| !character.is_whitespace())
                || document.layout.sources.iter().any(|source| {
                    !matches!(&source.key, ComposerSourceKey::Native)
                        && source.has_sendable_references()
                })
                || self
                    .drafts
                    .get(owner)
                    .is_some_and(|draft| draft.content.has_sendable_items()),
        )
    }
    #[cfg(test)]
    pub(crate) fn composer_documents_dirty(&self) -> bool {
        self.composer_documents
            .documents
            .values()
            .any(Document::dirty)
    }

    pub(crate) fn composer_documents_pending(&self) -> bool {
        !self.composer_documents.loads.is_empty()
            || !self.composer_documents.saves.is_empty()
            || !self.composer_documents.submission_retries.is_empty()
            || self
                .composer_documents
                .documents
                .values()
                .any(|document| document.append.is_some())
    }

    pub(crate) fn composer_document_ready(&self, owner: &DraftKey) -> bool {
        self.composer_documents.frozen.is_none()
            && self
                .composer_documents
                .documents
                .get(owner)
                .is_none_or(|document| {
                    document.ready_for_capture(&self.queue_storage.draft_companions(owner))
                })
    }

    pub(crate) fn composer_document_failed(&self, owner: &DraftKey) -> bool {
        self.composer_submission_failed(owner)
            || self
                .composer_documents
                .documents
                .get(owner)
                .is_some_and(|document| {
                    matches!(document.read, ReadState::Failed) || document.save_failure.is_some()
                })
    }

    pub(crate) fn composer_document_save_failed(&self, owner: &DraftKey) -> bool {
        self.composer_submission_failed(owner)
            || self
                .composer_documents
                .documents
                .get(owner)
                .and_then(|document| document.save_failure.as_ref())
                .map(|failure| &failure.error)
                .is_some()
    }

    pub(crate) fn record_native_composer_change(&mut self, owner: &DraftKey) {
        let revision = self
            .composer_documents
            .native_revisions
            .entry(owner.clone())
            .or_insert(1);
        *revision = revision.saturating_add(1);
    }

    pub(crate) fn unfreeze_composer_documents(&mut self) {
        self.composer_documents.frozen = None;
        self.composer_documents.epoch = self.composer_documents.epoch.saturating_add(1);
    }

    /// Navigation may leave an already-loaded result deferred behind an old IME
    /// preedit. Inactive documents can reconcile without touching the textarea.
    pub(crate) fn reconcile_composer_documents_for_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owners: Vec<_> = self.composer_documents.documents.keys().cloned().collect();
        for owner in owners {
            self.apply_deferred_composer(&owner, window, cx);
            self.reconcile_composer_references(&owner, window, cx);
        }
    }

    pub(crate) fn freeze_composer_documents(
        &mut self,
        cx: &App,
    ) -> Result<Vec<ComposerCapture>, QueueCaptureFailure> {
        self.save_composer_draft(cx);
        // A loading document contains only temporary editor state. Its previous
        // durable layout must remain authoritative until hydration reconciles it.
        if self
            .composer_documents
            .documents
            .iter()
            .any(|(owner, document)| {
                !document.ready_for_capture(&self.queue_storage.draft_companions(owner))
            })
        {
            return Err(QueueCaptureFailure::Unavailable);
        }
        // Cancel only UI waiters. Already accepted worker jobs retain their exact
        // prepared roots and drain independently. Complete puts may be observed
        // again; an actual partial file still requires explicit Retry authority.
        self.composer_documents.saves.clear();
        let owners: Vec<_> = self.composer_documents.documents.keys().cloned().collect();
        let mut captures = Vec::new();
        for owner in owners {
            let capture = self.capture_composer_owner(&owner)?;
            if capture.snapshot.is_some() {
                captures.push(capture);
            }
        }
        self.composer_documents.epoch = self
            .composer_documents
            .epoch
            .checked_add(1)
            .filter(|epoch| *epoch < u64::MAX)
            .ok_or(QueueCaptureFailure::Unavailable)?;
        let epoch = self.composer_documents.epoch;
        self.composer_documents.frozen = Some(epoch);
        for capture in &mut captures {
            capture.close = Some(ComposerClosePermit {
                epoch,
                owner: capture.composer.native.owner.clone(),
                generation: capture.generation,
                revision: capture.revision,
                reference: None,
            });
        }
        Ok(captures)
    }

    pub(crate) fn composer_capture_needs_flush(&self, capture: &ComposerCapture) -> bool {
        self.composer_documents
            .documents
            .get(&capture.composer.native.owner)
            .is_some_and(|document| {
                document.generation == capture.generation
                    && document.revision == capture.revision
                    && document.dirty()
            })
    }

    pub(crate) fn composer_edit_save_allowed(
        &self,
        edit: &ComposerEditRef,
        permit: Option<&ComposerClosePermit>,
    ) -> bool {
        if !self.persistence_loaded() || self.persistence_closing() {
            return false;
        }
        let Some(operation) = self.queue_storage.operation(&edit.operation) else {
            return false;
        };
        let Ok(owner) = operation.owner().draft_key() else {
            return false;
        };
        let Some(document) = self.composer_documents.documents.get(&owner) else {
            return false;
        };
        if document.generation != edit.generation
            || document.revision < edit.revision
            || !self.queue_storage.owns_composer_edit_write(&owner, edit)
        {
            return false;
        }
        match permit {
            Some(permit) => {
                self.composer_close_in_progress()
                    && permit.matches(&owner, edit, self.composer_documents.frozen)
                    && document.revision == edit.revision
            }
            None => {
                (!self.close_in_progress() || self.composer_hydration_close_in_progress())
                    && self.composer_documents.frozen.is_none()
            }
        }
    }

    fn capture_composer_owner(
        &self,
        owner: &DraftKey,
    ) -> Result<ComposerCapture, QueueCaptureFailure> {
        let native = self.drafts.get(owner).cloned().unwrap_or_default();
        let composer = self.capture_draft_companions(owner, native);
        let Some(document) = self.composer_documents.documents.get(owner) else {
            let generation = self
                .composer_documents
                .generations
                .get(owner)
                .copied()
                .unwrap_or(1);
            let revision = self
                .composer_documents
                .native_revisions
                .get(owner)
                .copied()
                .unwrap_or(1);
            if generation == 0 || generation == u64::MAX || revision == 0 || revision == u64::MAX {
                return Err(QueueCaptureFailure::Unavailable);
            }
            return if composer.references.is_empty() {
                Ok(ComposerCapture {
                    composer,
                    generation,
                    revision,
                    edit: None,
                    snapshot: None,
                    planned: None,
                    close: None,
                    explicit_retry: false,
                    native_authority: Some(self.composer_documents.native_authority.clone()),
                })
            } else {
                Err(QueueCaptureFailure::Unavailable)
            };
        };
        if !document.ready_for_capture(&composer.references) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let dirty = document.dirty();
        Ok(ComposerCapture {
            composer,
            generation: document.generation,
            revision: document.revision,
            edit: if dirty {
                None
            } else {
                document.durable.clone()
            },
            snapshot: dirty.then(|| {
                (
                    document.text.clone(),
                    document.selection.clone(),
                    document.layout.clone(),
                    document.known.clone(),
                )
            }),
            planned: document.planned.clone().filter(|planned| {
                (planned.generation, planned.revision) == (document.generation, document.revision)
            }),
            close: None,
            explicit_retry: false,
            native_authority: None,
        })
    }

    pub(crate) fn capture_composer_submission(
        &mut self,
        cx: &App,
    ) -> Result<ComposerCapture, QueueCaptureFailure> {
        self.save_composer_draft(cx);
        let owner = self
            .composer_key
            .clone()
            .ok_or(QueueCaptureFailure::Unavailable)?;
        if !self.composer_documents.documents.contains_key(&owner) {
            self.composer_documents
                .native_revisions
                .entry(owner.clone())
                .or_insert(1);
        }
        self.composer_documents.saves.remove(&owner);
        self.capture_composer_owner(&owner)
    }

    /// Returns true when this owner has a compound document, including a failed
    /// local read. The native Draft must never receive the joined textarea value.
    pub(crate) fn save_compound_composer(&mut self, cx: &App) -> bool {
        let Some(owner) = self.composer_key.as_ref() else {
            return false;
        };
        let Some(document) = self.composer_documents.documents.get_mut(owner) else {
            return false;
        };
        let composer = self.composer.read(cx);
        let (text, selection) = composer.committed_snapshot();
        if text != document.text {
            // set_value is an explicit whole-document replacement, not an
            // inferred diff. Ordinary typing arrives through committed events.
            let replacement = match &document.append {
                Some(append) => InputReplacement {
                    range: document.text.len()..document.text.len(),
                    text: append.suffix.clone().into(),
                },
                None => InputReplacement {
                    range: 0..document.text.len(),
                    text: text.to_string().into(),
                },
            };
            let event = CommittedInputEdit {
                replacements: vec![replacement],
                before: composer.history_checkpoint(),
                after: composer.history_checkpoint(),
                text,
                selection: selection.clone(),
            };
            if document.changed(&event).is_err() {
                document.read = ReadState::Failed;
            }
        }
        document.select(selection);
        true
    }

    pub(crate) fn on_committed_composer_edit(
        &mut self,
        _composer: &gpui_kit::Entity<gpui_kit::component::input::TextareaState>,
        event: &CommittedInputEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.composer_documents.frozen.is_some() {
            return;
        }
        let Some(owner) = self.composer_key.clone() else {
            return;
        };
        if !self.composer_editable()
            && !self
                .composer_documents
                .documents
                .get(&owner)
                .is_some_and(|document| document.append.is_some())
        {
            return;
        }
        if let Some(document) = self.composer_documents.documents.get_mut(&owner) {
            if document.changed(event).is_err() {
                document.read = ReadState::Failed;
            }
            self.apply_deferred_composer(&owner, window, cx);
            self.reconcile_composer_references(&owner, window, cx);
            self.schedule_composer_document_save(&owner, window, cx);
        } else {
            self.record_native_composer_change(&owner);
            let mut draft = self.drafts.take(&owner).unwrap_or_default();
            draft.edit_text(event.text.to_string(), event.selection.clone());
            self.save_draft_preview(&owner, &draft, cx);
            self.drafts.save(owner, draft);
            self.queue_persistence(cx);
        }
        self.autocomplete_after_edit(event, cx);
        self.resume_composer_close(window, cx);
        cx.notify();
    }

    pub(crate) fn on_composer_composition_ended(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(owner) = self.composer_key.clone() else {
            return;
        };
        self.apply_deferred_composer(&owner, window, cx);
        self.reconcile_composer_references(&owner, window, cx);
        self.schedule_composer_document_save(&owner, window, cx);
        self.resume_composer_close(window, cx);
        cx.notify();
    }

    /// An in-flight read covers its captured refs only. A Take that completes
    /// during that read must be loaded after the first append has committed.
    fn reconcile_composer_references(
        &mut self,
        owner: &DraftKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let references = self.queue_storage.draft_companions(owner);
        if self
            .composer_documents
            .documents
            .get(owner)
            .is_some_and(|document| {
                matches!(document.read, ReadState::Ready)
                    && document.append.is_none()
                    && document.deferred.is_none()
                    && document.known != references
            })
        {
            self.refresh_composer_document(owner, window, cx);
        }
    }

    pub(crate) fn restore_compound_composer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(owner) = self.composer_key.clone() else {
            return false;
        };
        let references = self.queue_storage.draft_companions(&owner);
        if references.is_empty() && !self.composer_documents.documents.contains_key(&owner) {
            return false;
        }
        if !self.composer_documents.documents.contains_key(&owner) {
            let draft = self.drafts.get(&owner).cloned().unwrap_or_default();
            let durable = self.queue_storage.composer_edit(&owner).cloned();
            let revision = durable.as_ref().map_or(1, |edit| edit.revision);
            let generation = durable.as_ref().map_or_else(
                || {
                    self.composer_documents
                        .generations
                        .get(&owner)
                        .copied()
                        .unwrap_or(1)
                },
                |edit| edit.generation,
            );
            let initial = if durable.is_some() {
                Draft::default()
            } else {
                draft.clone()
            };
            self.composer_documents.documents.insert(
                owner.clone(),
                Document {
                    generation,
                    revision,
                    text: Rope::from(initial.text.as_str()),
                    selection: initial.selection.clone(),
                    layout: Layout::native(&initial),
                    known: Vec::new(),
                    durable,
                    read: ReadState::Ready,
                    history: HashMap::new(),
                    history_order: VecDeque::new(),
                    append: None,
                    deferred: None,
                    planned: None,
                    save_failure: None,
                },
            );
        }
        let document = &self.composer_documents.documents[&owner];
        let text = document.text.to_string();
        let selection = document.selection.clone();
        self.composer.update(cx, |composer, cx| {
            composer.set_value(text, window, cx);
            composer.set_selected_range(selection, cx);
        });
        let checkpoint = self.composer.read(cx).history_checkpoint();
        let document = self
            .composer_documents
            .documents
            .get_mut(&owner)
            .expect("retained compound owner");
        document.history.clear();
        document.history_order.clear();
        document.remember(checkpoint, document.layout.clone());
        self.refresh_composer_document(&owner, window, cx);
        true
    }

    pub(crate) fn refresh_composer_document(
        &mut self,
        owner: &DraftKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.composer_documents.frozen.is_some()
            || self.composer_documents.loads.contains_key(owner)
        {
            return;
        }
        if self
            .composer_documents
            .documents
            .get(owner)
            .is_some_and(|document| document.append.is_some())
        {
            return;
        }
        if self
            .composer_documents
            .documents
            .get(owner)
            .is_some_and(|document| document.deferred.is_some())
        {
            self.apply_deferred_composer(owner, window, cx);
            self.reconcile_composer_references(owner, window, cx);
            return;
        }
        if !self.composer_documents.documents.contains_key(owner) {
            // A Take completion adopts the existing editor and its undo stack.
            // Only navigation/hydration uses restore_compound_composer/set_value.
            if self.queue_storage.draft_companions(owner).is_empty() {
                return;
            }
            if self.composer_key.as_ref() == Some(owner) {
                self.save_composer_draft(cx);
            }
            let draft = self.drafts.get(owner).cloned().unwrap_or_default();
            let durable = self.queue_storage.composer_edit(owner).cloned();
            let generation = durable.as_ref().map_or_else(
                || {
                    self.composer_documents
                        .generations
                        .get(owner)
                        .copied()
                        .unwrap_or(1)
                },
                |edit| edit.generation,
            );
            let revision = durable.as_ref().map_or(1, |edit| edit.revision);
            let mut document = Document {
                generation,
                revision,
                text: Rope::from(draft.text.as_str()),
                selection: draft.selection.clone(),
                layout: Layout::native(&draft),
                known: Vec::new(),
                durable,
                read: ReadState::Ready,
                history: HashMap::new(),
                history_order: VecDeque::new(),
                append: None,
                deferred: None,
                planned: None,
                save_failure: None,
            };
            if self.composer_key.as_ref() == Some(owner) {
                document.remember(
                    self.composer.read(cx).history_checkpoint(),
                    document.layout.clone(),
                );
            }
            self.composer_documents
                .documents
                .insert(owner.clone(), document);
        }
        let references = self.queue_storage.draft_companions(owner);
        let document = self
            .composer_documents
            .documents
            .get_mut(owner)
            .expect("checked owner");
        if document.save_failure.is_some() {
            return;
        }
        if references == document.known && matches!(document.read, ReadState::Ready) {
            return;
        }
        let restore = document
            .known
            .is_empty()
            .then(|| document.durable.clone())
            .flatten();
        let missing = references
            .iter()
            .filter(|reference| !document.known.contains(reference))
            .cloned()
            .collect();
        let generation = document.generation;
        document.read = ReadState::Loading;
        let capture = CompanionCapture {
            native: QueueNativeSource {
                owner: owner.clone(),
                draft: self.drafts.get(owner).cloned().unwrap_or_default(),
            },
            references: missing,
        };
        let owner = owner.clone();
        let task_owner = owner.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let loaded = load(&this, cx, &capture, restore.as_ref(), references).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.composer_documents.loads.remove(&owner);
                let Some(document) = this
                    .composer_documents
                    .documents
                    .get_mut(&owner)
                    .filter(|document| document.generation == generation)
                else {
                    return;
                };
                match loaded {
                    Ok(loaded) => document.deferred = Some(loaded),
                    Err(_) => document.read = ReadState::Failed,
                }
                this.apply_deferred_composer(&owner, window, cx);
                this.reconcile_composer_references(&owner, window, cx);
                this.schedule_composer_document_save(&owner, window, cx);
                this.resume_composer_close(window, cx);
                cx.notify();
            });
        });
        self.composer_documents.loads.insert(task_owner, task);
    }

    fn apply_deferred_composer(
        &mut self,
        owner: &DraftKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.composer_key.as_ref() == Some(owner);
        if self.composer_documents.frozen.is_some()
            || (active && self.composer.read(cx).is_composing())
        {
            return;
        }
        let Some(document) = self.composer_documents.documents.get_mut(owner) else {
            return;
        };
        let Some(loaded) = document.deferred.take() else {
            return;
        };
        let restored_selection = document.stage_loaded(loaded);
        let Some(append) = &document.append else {
            return;
        };
        if active {
            let suffix = append.suffix.clone();
            self.composer.update(cx, |composer, cx| {
                composer.append_with_selection(suffix, restored_selection, window, cx);
            });
        } else {
            let append = document.append.take().expect("staged local append");
            document
                .text
                .replace(document.text.len()..document.text.len(), &append.suffix);
            document.layout = append.after;
            if let Some(selection) = restored_selection {
                document.selection = selection;
            }
            document.revision = document.revision.saturating_add(1);
            document.clear_unwritten_save_failure();
        }
    }

    fn schedule_composer_document_save(
        &mut self,
        owner: &DraftKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_in_progress()
            || self.composer_documents.frozen.is_some()
            || self.composer_documents.saves.contains_key(owner)
            || !self
                .composer_documents
                .documents
                .get(owner)
                .is_some_and(|document| {
                    document.dirty()
                        && document.ready_for_capture(&self.queue_storage.draft_companions(owner))
                })
        {
            return;
        }
        let owner = owner.clone();
        let task_owner = owner.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let capture = this
                .update_in(cx, |this, _, cx| {
                    if this.composer_key.as_ref() == Some(&owner) {
                        this.save_composer_draft(cx);
                    }
                    this.capture_composer_owner(&owner)
                })
                .ok()
                .and_then(Result::ok);
            let attempted = capture
                .as_ref()
                .map(|capture| (capture.generation, capture.revision));
            let result = match capture {
                Some(capture) => flush(&this, cx, capture).await.map(|_| ()),
                None => Err(QueueCaptureFailure::Unavailable),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_composer_autosave(&owner, attempted, result, window, cx);
            });
        });
        self.composer_documents.saves.insert(task_owner, task);
    }

    fn finish_composer_autosave(
        &mut self,
        owner: &DraftKey,
        attempted: Option<(u64, u64)>,
        result: Result<(), QueueCaptureFailure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer_documents.saves.remove(owner);
        if self
            .composer_documents
            .documents
            .get(owner)
            .is_some_and(|document| {
                document.dirty()
                    && (result.is_ok()
                        || attempted.is_some_and(|(generation, revision)| {
                            document.generation > generation
                                || (document.generation == generation
                                    && document.revision > revision)
                        }))
            })
        {
            self.schedule_composer_document_save(owner, window, cx);
        }
        self.resume_composer_close(window, cx);
        cx.notify();
    }

    /// Only this existing local Retry action authorizes a partial-put repair.
    /// Finish the exact failed snapshot before saving any newer editor revision.
    pub(crate) fn retry_composer_document(
        &mut self,
        owner: &DraftKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_in_progress()
            || self.composer_documents.frozen.is_some()
            || self.composer_documents.saves.contains_key(owner)
        {
            return;
        }
        let submission_failed = self.composer_submission_failed(owner);
        self.retry_composer_submissions(Some(owner), window, cx);
        let retry = self
            .composer_documents
            .documents
            .get(owner)
            .and_then(|document| {
                document
                    .save_failure
                    .as_ref()
                    .map(|failure| failure.capture.clone().for_explicit_retry())
            });
        let Some(capture) = retry else {
            if submission_failed {
                return;
            }
            self.refresh_composer_document(owner, window, cx);
            return;
        };
        let owner = owner.clone();
        let task_owner = owner.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = flush(&this, cx, capture).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.composer_documents.saves.remove(&owner);
                if result.is_ok() {
                    this.reconcile_composer_references(&owner, window, cx);
                    this.schedule_composer_document_save(&owner, window, cx);
                }
                this.resume_composer_close(window, cx);
                cx.notify();
            });
        });
        self.composer_documents.saves.insert(task_owner, task);
        cx.notify();
    }

    pub(crate) fn consume_composer_submission(
        &mut self,
        operation: &QueueOperationId,
        capture: &ComposerCapture,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), QueueCaptureFailure> {
        let owner = &capture.composer.native.owner;
        if capture.snapshot.is_some() {
            return Err(QueueCaptureFailure::Unavailable);
        }
        if self.composer_key.as_ref() == Some(owner) {
            self.save_composer_draft(cx);
        }
        let consume = match self.composer_documents.documents.get(owner) {
            Some(document) => {
                capture.edit.is_some()
                    && (document.generation, document.revision)
                        == (capture.generation, capture.revision)
                    && document.ready_for_capture(&capture.composer.references)
            }
            None => {
                capture.edit.is_none()
                    && capture.composer.references.is_empty()
                    && capture.native_authority.as_ref().is_some_and(|authority| {
                        Arc::ptr_eq(authority, &self.composer_documents.native_authority)
                    })
                    && self
                        .composer_documents
                        .generations
                        .get(owner)
                        .copied()
                        .unwrap_or(1)
                        == capture.generation
                    && self.composer_documents.native_revisions.get(owner)
                        == Some(&capture.revision)
                    && self
                        .drafts
                        .get(owner)
                        .is_some_and(|draft| draft.same_content(&capture.composer.native.draft))
            }
        };
        let mut remaining = self.drafts.get(owner).cloned().unwrap_or_default();
        let consume = consume && remaining.consume_captured(&capture.composer.native.draft);
        let consumption = if consume {
            ComposerConsumption::Consume
        } else {
            ComposerConsumption::PreserveCurrent
        };
        self.change_queue_storage(|queue, workspace| {
            queue.consume_composer_capture(
                operation,
                owner,
                &capture.composer.references,
                capture.edit.as_ref(),
                consumption,
                |candidate| workspace.validate_queue_candidate(candidate),
            )
        })?;
        // The captured send is now prepared. A later revision is independent
        // input and retains its exact text, rich roots and toolkit undo history.
        if !consume {
            return Ok(());
        }
        self.composer_documents.documents.remove(owner);
        self.composer_documents.saves.remove(owner);
        self.composer_documents
            .generations
            .insert(owner.clone(), capture.generation.saturating_add(1));
        self.composer_documents.native_revisions.remove(owner);
        self.drafts.save(owner.clone(), remaining);
        if self.composer_key.as_ref() == Some(owner) {
            self.restore_composer_draft(window, cx);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn clear_composer_document(
        &mut self,
        owner: &DraftKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), QueueCaptureFailure> {
        if self.close_in_progress() || self.composer_documents.frozen.is_some() {
            return Err(QueueCaptureFailure::Unavailable);
        }
        let generation = self.composer_documents.documents.get(owner).map_or_else(
            || {
                self.composer_documents
                    .generations
                    .get(owner)
                    .copied()
                    .unwrap_or(1)
            },
            |document| document.generation,
        );
        let next = generation
            .checked_add(1)
            .filter(|generation| *generation < u64::MAX)
            .ok_or(QueueCaptureFailure::Invalid)?;
        self.change_queue_storage(|queue, workspace| {
            queue.clear_composer_document(owner, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })?;
        self.composer_documents.documents.remove(owner);
        self.composer_documents
            .generations
            .insert(owner.clone(), next);
        self.composer_documents.native_revisions.remove(owner);
        self.composer_documents.loads.remove(owner);
        self.composer_documents.saves.remove(owner);
        self.drafts.take(owner);
        if self.composer_key.as_ref() == Some(owner) {
            self.restore_composer_draft(window, cx);
        }
        self.queue_persistence(cx);
        Ok(())
    }
}

async fn load(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    capture: &CompanionCapture,
    restore: Option<&ComposerEditRef>,
    references: Vec<DraftCompanionRef>,
) -> Result<Loaded, QueueCaptureFailure> {
    let snapshot = if let Some(reference) = restore {
        Some(read_edit(this, cx, reference, &capture.native.owner).await?)
    } else {
        None
    };
    let keys = source_keys(this, cx, &capture.references).await?;
    let items = draft_companion::materialize(this, cx, capture).await?;
    cx.background_executor()
        .spawn(async move {
            if items.len() != keys.len() {
                return Err(QueueCaptureFailure::Invalid);
            }
            let mut headers = Vec::new();
            for (item, key) in items.into_iter().zip(keys) {
                let ids = item.indexed.index().context_ids.clone();
                let local = QueueLocalCapture::from_document(item.document, ids, sha256)?;
                headers.push((
                    key.clone(),
                    local.item().content.clone(),
                    SourceState::local(key, local.item()),
                ));
            }
            Ok(Loaded {
                snapshot,
                headers,
                references,
            })
        })
        .await
}

async fn source_keys(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    references: &[DraftCompanionRef],
) -> Result<Vec<ComposerSourceKey>, QueueCaptureFailure> {
    this.update_in(cx, |this, _, _| {
        let mut keys = Vec::new();
        for reference in references {
            let local = this
                .queue_storage
                .local_take(&reference.operation)
                .ok_or(QueueCaptureFailure::Unavailable)?;
            for item in 0..local.transfer.items.len() {
                keys.push(ComposerSourceKey::Companion {
                    operation: reference.operation.clone(),
                    companion: reference.companion.clone(),
                    item,
                });
            }
        }
        Ok(keys)
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?
}

/// Reads only committed local captures. Text-edited inputs keep delivery empty
/// until captured-owner preparation; originals stay immutable.
pub(crate) async fn materialize(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    capture: &ComposerCapture,
) -> Result<ComposerInputs, QueueCaptureFailure> {
    if capture.snapshot.is_some() {
        return Err(QueueCaptureFailure::Unavailable);
    }
    let snapshot = match &capture.edit {
        Some(edit) => Some(read_edit(this, cx, edit, &capture.composer.native.owner).await?),
        None => None,
    };
    let keys = source_keys(this, cx, &capture.composer.references).await?;
    let items = draft_companion::materialize(this, cx, &capture.composer).await?;
    let native = capture.composer.native.clone();
    cx.background_executor()
        .spawn(async move {
            let Some(snapshot) = snapshot else {
                let mut text = vec![ComposerTextPiece::Source(0)];
                let mut has_display = !native.draft.text.is_empty();
                for (ordinal, item) in items.iter().enumerate() {
                    let content = match item.document.item() {
                        QueuePayloadItem::Manifest(item) => &item.content,
                        QueuePayloadItem::Legacy(item) => &item.content,
                    };
                    if has_display && !content.is_empty() {
                        text.push(ComposerTextPiece::Literal {
                            text: "\n\n".into(),
                            edited: false,
                        });
                    }
                    text.push(ComposerTextPiece::Source(ordinal + 1));
                    has_display |= !content.is_empty();
                }
                return Ok(ComposerInputs {
                    native,
                    companions: items
                        .into_iter()
                        .map(|item| ComposerCompanionInput::Original(Box::new(item)))
                        .collect(),
                    text,
                    exclusions: Vec::new(),
                });
            };
            if keys.len() != items.len() {
                return Err(QueueCaptureFailure::Invalid);
            }
            let rope = Rope::from(snapshot.text.as_str());
            let mut native = native;
            native.draft.text = snapshot.layout.display(&rope, &ComposerSourceKey::Native);
            native.draft.selection = 0..0;
            let native_state = snapshot
                .layout
                .sources
                .iter()
                .find(|state| state.key == ComposerSourceKey::Native);
            if let Some(state) = native_state {
                if state.agents.len() != native.draft.content.agents.len()
                    || state.skills.len() != native.draft.content.skills.len()
                {
                    return Err(QueueCaptureFailure::Invalid);
                }
                for (agent, anchor) in native.draft.content.agents.iter_mut().zip(&state.agents) {
                    agent.mention = anchor
                        .as_ref()
                        .map(|anchor| {
                            Ok::<_, QueueCaptureFailure>(
                                openchamber_native_core::draft_content::Mention {
                                    start: u32::try_from(anchor.start)
                                        .map_err(|_| QueueCaptureFailure::Invalid)?,
                                    end: u32::try_from(anchor.end)
                                        .map_err(|_| QueueCaptureFailure::Invalid)?,
                                    text: anchor.text.to_string(),
                                },
                            )
                        })
                        .transpose()?;
                }
                retain_ordinals(&mut native.draft.content.files, &state.removed_files);
                retain_ordinals(&mut native.draft.content.agents, &state.removed_agents);
                retain_ordinals(&mut native.draft.content.skills, &state.removed_skills);
            } else {
                for agent in &mut native.draft.content.agents {
                    agent.mention = None;
                }
            }
            let mut companions = Vec::new();
            let mut exclusions = Vec::new();
            for (item, key) in items.into_iter().zip(&keys) {
                let state = snapshot
                    .layout
                    .sources
                    .iter()
                    .find(|state| &state.key == key);
                let present = snapshot.layout.contains_source(key);
                let display = snapshot.layout.display(&rope, key);
                if present && state.is_some_and(|state| !state.edited && !state.has_removals()) {
                    let original = match item.document.item() {
                        QueuePayloadItem::Manifest(item) => &item.content,
                        QueuePayloadItem::Legacy(item) => &item.content,
                    };
                    if &display != original {
                        return Err(QueueCaptureFailure::Invalid);
                    }
                    companions.push(ComposerCompanionInput::Original(Box::new(item)));
                    continue;
                }
                queue_take::validate_file_bindings(&item)?;
                let mut files = ordered_files(&item)?;
                let ids = item.indexed.index().context_ids.clone();
                let original =
                    QueueLocalCapture::from_document(item.document, ids.clone(), sha256)?;
                let mut local = original.item().clone();
                let reprocess_text = if let Some(state) = state {
                    exclusions.push(state.exclusions(companions.len() + 1, &local));
                    retain_ordinals(&mut files, &state.removed_files);
                    state.project_display(&mut local, &display, present)?
                } else {
                    for file in &mut local.files {
                        file.mention = None;
                    }
                    for file in &mut local.prompt_parts.file_references {
                        file.mention = None;
                    }
                    for agent in &mut local.prompt_parts.agents {
                        agent.mention = None;
                    }
                    for skill in &mut local.prompt_parts.skills {
                        skill.mention = None;
                    }
                    local.content = display;
                    local.text = String::new();
                    true
                };
                let capture = QueueLocalCapture::new(original.directory(), local, ids, sha256)?;
                companions.push(ComposerCompanionInput::Edited {
                    capture: Box::new(capture),
                    files,
                    reprocess_text,
                });
            }
            let mut text = Vec::new();
            let mut offset = 0;
            for span in &snapshot.layout.spans {
                match &span.kind {
                    SpanKind::Source(ComposerSourceKey::Native) => {
                        text.push(ComposerTextPiece::Source(0))
                    }
                    SpanKind::Source(key) => text.push(ComposerTextPiece::Source(
                        keys.iter()
                            .position(|source| source == key)
                            .ok_or(QueueCaptureFailure::Invalid)?
                            + 1,
                    )),
                    SpanKind::Literal { edited } => text.push(ComposerTextPiece::Literal {
                        text: rope.slice(offset..offset + span.bytes).to_string(),
                        edited: *edited,
                    }),
                }
                offset += span.bytes;
            }
            Ok(ComposerInputs {
                native,
                companions,
                text,
                exclusions,
            })
        })
        .await
}

fn ordered_files(item: &MaterializedQueueItem) -> Result<Vec<BlobDescriptor>, QueueCaptureFailure> {
    use crate::queue_storage::QueueFileBinding;
    match item.document.item() {
        QueuePayloadItem::Manifest(manifest) => manifest.attachment_manifest.iter().map(|file| {
            item.retained.files.iter().find(|binding| matches!(binding, QueueFileBinding::Manifest { blob, .. } if blob == &file.attachment.blob_id))
                .ok_or(QueueCaptureFailure::Invalid)?.descriptor().map_err(Into::into)
        }).collect(),
        QueuePayloadItem::Legacy(legacy) => (0..legacy.attachments.len()).map(|index| {
            item.retained.files.iter().find(|binding| matches!(binding, QueueFileBinding::Legacy { ordinal, .. } if *ordinal == index))
                .ok_or(QueueCaptureFailure::Invalid)?.descriptor().map_err(Into::into)
        }).collect(),
    }
}

async fn read_edit(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    reference: &ComposerEditRef,
    owner: &DraftKey,
) -> Result<EditDocument, QueueCaptureFailure> {
    let bytes = queue_payload_io::read(this, cx, &reference.object).await?;
    let reference = reference.clone();
    let owner = owner.clone();
    cx.background_executor()
        .spawn(async move {
            if bytes.len() as u64 != reference.byte_len || sha256(&bytes) != reference.sha256 {
                return Err(QueueCaptureFailure::Invalid);
            }
            let snapshot: EditDocument =
                serde_json::from_slice(&bytes).map_err(|_| QueueCaptureFailure::Invalid)?;
            snapshot.validate(&owner, &reference)?;
            Ok(snapshot)
        })
        .await
}

async fn save_edit(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    edit: &ComposerEditRef,
    permit: Option<&ComposerClosePermit>,
) -> Result<(), QueueCaptureFailure> {
    let barrier = this
        .update_in(cx, |this, _, cx| {
            this.composer_edit_save_allowed(edit, permit)
                .then(|| this.persistence_barrier(cx))
                .flatten()
        })
        .ok()
        .flatten();
    queue_payload_io::durable(this, cx, barrier).await
}

pub(crate) async fn flush(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    mut capture: ComposerCapture,
) -> Result<ComposerCapture, QueueCaptureFailure> {
    let result = flush_snapshot(this, cx, &mut capture).await;
    let _ = this.update_in(cx, |this, _, cx| {
        let Some(document) = this
            .composer_documents
            .documents
            .get_mut(&capture.composer.native.owner)
            .filter(|document| document.generation == capture.generation)
        else {
            return;
        };
        match &result {
            Ok(()) => {
                if document
                    .save_failure
                    .as_ref()
                    .is_some_and(|failure| failure.capture.revision <= capture.revision)
                {
                    document.save_failure = None;
                }
            }
            Err(error) => {
                let error = match error {
                    QueueCaptureFailure::Unavailable => QueueCaptureFailure::Unavailable,
                    QueueCaptureFailure::Invalid => QueueCaptureFailure::Invalid,
                    QueueCaptureFailure::Persistence => QueueCaptureFailure::Persistence,
                    QueueCaptureFailure::Transport => QueueCaptureFailure::Transport,
                };
                document.save_failed(capture.clone(), error);
            }
        }
        cx.notify();
    });
    result.map(|()| capture)
}

async fn flush_snapshot(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    capture: &mut ComposerCapture,
) -> Result<(), QueueCaptureFailure> {
    let Some((text, selection, layout, shown)) = capture.snapshot.clone() else {
        return Ok(());
    };
    let owner = capture.composer.native.owner.clone();
    let stored_owner = Owner::capture(&owner);
    let generation = capture.generation;
    let revision = capture.revision;
    let companions = capture.composer.references.clone();
    let bytes = cx
        .background_executor()
        .spawn(async move {
            let document = EditDocument {
                version: 1,
                owner: stored_owner,
                generation,
                revision,
                companions,
                shown,
                text: text.to_string(),
                start: selection.start,
                end: selection.end,
                layout,
            };
            serde_json::to_vec(&document).map_err(|_| QueueCaptureFailure::Invalid)
        })
        .await?;
    let operation = capture
        .composer
        .references
        .first()
        .ok_or(QueueCaptureFailure::Unavailable)?
        .operation
        .clone();
    let (object, ids) = this
        .update_in(cx, |this, _, _| {
            let document = this
                .composer_documents
                .documents
                .get(&owner)
                .ok_or(QueueCaptureFailure::Unavailable)?;
            if document.generation != capture.generation
                || document.revision < capture.revision
                || this.persistence_closing()
            {
                return Err(QueueCaptureFailure::Unavailable);
            }
            if let Some(planned) = &capture.planned {
                let object = this
                    .queue_storage
                    .object(&planned.object)
                    .ok_or(QueueCaptureFailure::Unavailable)?;
                let ids = object
                    .reference()
                    .chunks()
                    .iter()
                    .map(|chunk| chunk.descriptor().map(|descriptor| *descriptor.id()))
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok((planned.object.clone(), ids));
            }
            let nonce = this.message_ids.next().0.to_string();
            let object = QueueObjectId::new(&format!("{}-edit-{nonce}", operation.as_str()))?;
            let ids = (0..bytes.len().div_ceil(QUEUE_CHUNK_BYTES))
                .map(|_| {
                    let digest = sha256(this.message_ids.next().0.as_str().as_bytes());
                    let label = digest[..16]
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>();
                    BlobId::parse(&label).map_err(|_| QueueCaptureFailure::Invalid)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, QueueCaptureFailure>((object, ids))
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    let (bytes, reference) = cx
        .background_executor()
        .spawn(async move {
            let chunks = bytes
                .chunks(QUEUE_CHUNK_BYTES)
                .zip(ids)
                .map(|(chunk, id)| BlobDescriptor::new(id, chunk.len() as u64, sha256(chunk)))
                .collect();
            let reference = QueueSpoolRef::new(
                bytes.len() as u64,
                sha256(&bytes),
                chunks,
                QueueObjectKind::NativeCompanion,
            )?;
            Ok::<_, QueueCaptureFailure>((bytes, reference))
        })
        .await?;
    let edit = ComposerEditRef {
        operation: operation.clone(),
        object: object.clone(),
        generation: capture.generation,
        revision: capture.revision,
        byte_len: reference.total_bytes(),
        sha256: *reference.sha256(),
        companions: capture.composer.references.clone(),
    };
    if let Some(permit) = &mut capture.close {
        permit.reference = Some(edit.clone());
    }
    this.update_in(cx, |this, _, _| {
        if let Some(permit) = &capture.close {
            if !this.composer_close_in_progress()
                || !permit.matches(&owner, &edit, this.composer_documents.frozen)
                || !this
                    .composer_documents
                    .documents
                    .get(&owner)
                    .is_some_and(|document| {
                        (document.generation, document.revision) == (edit.generation, edit.revision)
                    })
            {
                return Err(QueueCaptureFailure::Unavailable);
            }
        } else if (this.close_in_progress() && !this.composer_hydration_close_in_progress())
            || this.composer_documents.frozen.is_some()
        {
            return Err(QueueCaptureFailure::Unavailable);
        }
        if let Some(planned) = &capture.planned {
            if planned != &edit {
                return Err(QueueCaptureFailure::Invalid);
            }
        } else {
            let object = QueueObject::planned(
                object.clone(),
                operation,
                QueueObjectKind::NativeCompanion,
                capture.generation,
                reference,
            )?;
            this.change_queue_storage(|queue, workspace| {
                queue.plan_composer_edit(&owner, edit.clone(), object, |candidate| {
                    workspace.validate_queue_candidate(candidate)
                })
            })?;
        }
        // Preserve this flush's exact accepted plan even if a newer save later
        // replaces the document's current planned slot before this put fails.
        capture.planned = Some(edit.clone());
        if !this.composer_edit_save_allowed(&edit, capture.close.as_ref()) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        if let Some(document) = this.composer_documents.documents.get_mut(&owner) {
            document.planned = Some(edit.clone());
        }
        Ok::<_, QueueCaptureFailure>(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::persist_composer_edit(
        this,
        cx,
        &edit,
        bytes,
        capture.close.as_ref(),
        capture.explicit_retry,
    )
    .await?;
    this.update_in(cx, |this, _, _| {
        if !this.composer_edit_save_allowed(&edit, capture.close.as_ref()) {
            return Err(QueueCaptureFailure::Unavailable);
        }
        this.change_queue_storage(|queue, workspace| {
            queue.publish_composer_edit(&owner, &edit, |candidate| {
                workspace.validate_queue_candidate(candidate)
            })
        })?;
        Ok::<_, QueueCaptureFailure>(())
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)??;
    save_edit(this, cx, &edit, capture.close.as_ref()).await?;
    this.update_in(cx, |this, _, _| {
        if let Some(document) = this
            .composer_documents
            .documents
            .get_mut(&owner)
            .filter(|document| document.generation == edit.generation)
        {
            if document
                .durable
                .as_ref()
                .is_none_or(|durable| durable.revision <= edit.revision)
            {
                document.durable = Some(edit.clone());
            }
            if document.planned.as_ref() == Some(&edit) {
                document.planned = None;
            }
        }
    })
    .map_err(|_| QueueCaptureFailure::Unavailable)?;
    if capture.close.is_none()
        && this
            .update_in(cx, |this, _, _| !this.close_in_progress())
            .unwrap_or(false)
    {
        retire_unrooted_edits(this, cx, &owner).await?;
    }
    capture.edit = Some(edit);
    capture.planned = None;
    capture.snapshot = None;
    Ok(())
}

/// Explicit terminal/discard cleanup. The ledger retains every version still
/// named by the current draft or a consuming Enqueue/Send/Prefix operation.
/// Call after retiring that consumer and before retiring its original Takes.
pub(crate) async fn retire_unrooted_edits(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    owner: &DraftKey,
) -> Result<(), QueueCaptureFailure> {
    let chunks = this
        .update_in(cx, |this, _, _| {
            if this.close_in_progress() {
                return Err(QueueCaptureFailure::Unavailable);
            }
            let previous: HashSet<_> = this.queue_storage.retired_blobs()?.into_iter().collect();
            this.change_queue_storage(|queue, workspace| {
                queue.retire_composer_edits(owner, |candidate| {
                    workspace.validate_queue_candidate(candidate)
                })
            })?;
            Ok::<_, QueueCaptureFailure>(
                this.queue_storage
                    .retired_blobs()?
                    .into_iter()
                    .filter(|id| !previous.contains(id))
                    .collect(),
            )
        })
        .map_err(|_| QueueCaptureFailure::Unavailable)??;
    queue_payload_io::save(this, cx).await?;
    crate::queue_enqueue::release_retirement(
        this,
        cx,
        crate::queue_enqueue::QueueRetirement {
            chunks,
            attachments: Vec::new(),
        },
    )
    .await
}

#[cfg(test)]
#[path = "composer_document_tests.rs"]
mod tests;
