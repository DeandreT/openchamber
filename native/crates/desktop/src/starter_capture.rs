//! Complete starter ownership while a read-only prompt-override request is pending.
//!
//! The composer and a capture are independent owners. Restart recovery restores
//! captures to their original drafts and never resumes activation or creates a
//! session. Prepared bytes remain owned by the attachment catalog; each capture's
//! full source draft is another root in that catalog's reachability validation.

use std::collections::{HashMap, HashSet};
use std::fmt;

use openchamber_native_core::draft::{self, ContentConflict, Draft, DraftKey, DraftStore};
use openchamber_native_core::ids::MessageId;
use openchamber_native_core::permission_mode::PermissionMode;
use openchamber_native_core::send_config::Selection;
use openchamber_native_transport::prompt_parts::{PromptParts, SyntheticContext};

/// Immutable source content and configuration captured before reading overrides.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StarterCapture {
    /// Also the admission identity of the starter's instructions after rendering.
    pub id: MessageId,
    pub owner: DraftKey,
    pub project: String,
    /// Exact source text, selection and semantic content, without the preset.
    pub draft: Draft,
    /// What a definite creation failure restores, with the starter prepended.
    pub preset_text: String,
    pub selection: Selection,
    /// Files and ordered context converted once at activation, before any await.
    pub parts: PromptParts,
    pub permission_mode: Option<PermissionMode>,
}

impl fmt::Debug for StarterCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StarterCapture")
            .field("id", &self.id)
            .field("draft", &self.draft)
            .finish_non_exhaustive()
    }
}

impl StarterCapture {
    pub(crate) fn restore_selection(
        &self,
        selections: &mut openchamber_native_core::composer_selection::ComposerSelections,
    ) {
        selections.restore_if_absent(
            openchamber_native_core::composer_selection::SelectionOwner {
                draft: self.owner.clone(),
                project: Some(openchamber_native_core::ids::ProjectId::from(
                    self.project.clone(),
                )),
            },
            self.selection.clone(),
        );
    }

    /// Retains every original semantic value in the preset rollback draft.
    /// Textual mentions move with the trimmed source text's explicit prefix.
    pub(crate) fn rollback_draft(&self) -> Result<Draft, ContentConflict> {
        let trimmed = self.draft.text.trim();
        let prefix_bytes = self
            .preset_text
            .len()
            .checked_sub(trimmed.len())
            .filter(|offset| self.preset_text.get(*offset..) == Some(trimmed))
            .ok_or(ContentConflict::MentionOffsetOverflow)?;
        let removed_bytes = self.draft.text.len() - self.draft.text.trim_start().len();
        let removed = u32::try_from(self.draft.text[..removed_bytes].encode_utf16().count())
            .map_err(|_| ContentConflict::MentionOffsetOverflow)?;
        let prefix = u32::try_from(self.preset_text[..prefix_bytes].encode_utf16().count())
            .map_err(|_| ContentConflict::MentionOffsetOverflow)?;
        let mut draft = self.draft.clone();
        for agent in &mut draft.content.agents {
            if let Some(mention) = &mut agent.mention {
                mention.start = mention
                    .start
                    .checked_sub(removed)
                    .and_then(|start| start.checked_add(prefix))
                    .ok_or(ContentConflict::MentionOffsetOverflow)?;
                mention.end = mention
                    .end
                    .checked_sub(removed)
                    .and_then(|end| end.checked_add(prefix))
                    .ok_or(ContentConflict::MentionOffsetOverflow)?;
            }
        }
        draft.text = self.preset_text.clone();
        let end = draft.text.len();
        draft.selection = end..end;
        Ok(draft)
    }

    /// Starter instructions precede the source draft's original context order.
    /// Rendering changes the visible text's layout, so agent references travel
    /// without source-text ranges. The exact ranges remain in the rollback draft.
    pub(crate) fn rendered_parts(&self, instructions: String) -> PromptParts {
        let mut parts = self.parts.clone();
        let mut names = HashSet::new();
        parts.agents.retain_mut(|agent| {
            agent.mention = None;
            names.insert(agent.name.clone())
        });
        parts.context.insert(
            0,
            SyntheticContext {
                id: self.id.clone(),
                text: instructions,
                description: None,
                metadata: None,
            },
        );
        parts
    }
}

/// Successful restorations have moved to `DraftStore`; conflicts remain captured.
pub(crate) struct StarterDrain {
    pub restored: Vec<StarterCapture>,
    pub conflicts: Vec<(MessageId, ContentConflict)>,
}

#[derive(Default)]
pub(crate) struct StarterCaptures {
    entries: HashMap<MessageId, StarterCapture>,
}

impl StarterCaptures {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &StarterCapture> {
        self.entries.values()
    }

    pub(crate) fn can_begin(&self, owner: &DraftKey) -> bool {
        !self.entries.values().any(|capture| &capture.owner == owner)
    }

    /// Transfers a complete source draft synchronously. Failure changes no owner.
    pub(crate) fn begin(&mut self, capture: StarterCapture, drafts: &mut DraftStore) -> bool {
        let unchanged = match drafts.get(&capture.owner) {
            Some(draft) => draft == &capture.draft,
            None => capture.draft == Draft::default(),
        };
        if self.entries.contains_key(&capture.id) || !self.can_begin(&capture.owner) || !unchanged {
            return false;
        }
        drafts.take(&capture.owner);
        self.entries.insert(capture.id.clone(), capture);
        true
    }

    pub(crate) fn get(&self, id: &MessageId) -> Option<&StarterCapture> {
        self.entries.get(id)
    }

    pub(crate) fn discard_owner(&mut self, owner: &DraftKey) -> Vec<StarterCapture> {
        let ids: Vec<_> = self
            .entries
            .values()
            .filter(|capture| &capture.owner == owner)
            .map(|capture| capture.id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| self.entries.remove(&id))
            .collect()
    }

    /// Call only after a creation owns the complete rollback draft and resources.
    pub(crate) fn handoff(&mut self, id: &MessageId) -> Option<StarterCapture> {
        self.entries.remove(id)
    }

    /// Restores the original source, including when its composer is inactive.
    /// A conflict retains the immutable capture and leaves newer content intact.
    pub(crate) fn restore_draft(
        &mut self,
        id: &MessageId,
        drafts: &mut DraftStore,
    ) -> Result<Option<StarterCapture>, ContentConflict> {
        let Some(capture) = self.entries.get(id) else {
            return Ok(None);
        };
        let restored = draft::restore_failed(drafts.get(&capture.owner), &capture.draft)?;
        drafts.save(capture.owner.clone(), restored);
        Ok(self.entries.remove(id))
    }

    pub(crate) fn snapshot(&self) -> Vec<StarterCapture> {
        let mut captures: Vec<_> = self.entries.values().cloned().collect();
        captures.sort_by(|left, right| left.id.cmp(&right.id));
        captures
    }

    /// Receives fully validated journal entries. This never starts a request.
    pub(crate) fn restore(captures: Vec<StarterCapture>) -> Self {
        Self {
            entries: captures
                .into_iter()
                .map(|capture| (capture.id.clone(), capture))
                .collect(),
        }
    }

    /// Hydration or explicit recovery merges original drafts without replaying.
    /// Call before enabling editing on startup; publish retained conflicts through
    /// the existing recovery presentation rather than discarding their contents.
    pub(crate) fn drain(&mut self, drafts: &mut DraftStore) -> StarterDrain {
        let ids: Vec<_> = self.entries.keys().cloned().collect();
        let mut drained = StarterDrain {
            restored: Vec::new(),
            conflicts: Vec::new(),
        };
        for id in ids {
            match self.restore_draft(&id, drafts) {
                Ok(Some(capture)) => drained.restored.push(capture),
                Ok(None) => {}
                Err(conflict) => drained.conflicts.push((id, conflict)),
            }
        }
        drained
    }
}

#[cfg(test)]
#[path = "starter_capture_tests.rs"]
mod tests;
