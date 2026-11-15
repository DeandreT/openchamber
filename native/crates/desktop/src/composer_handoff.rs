//! Text and hidden parts another surface hands to the composer for the user
//! to send (`setPendingInputText` with `setPendingSyntheticParts`, and
//! `openNewSessionDraft` with an initial prompt and synthetic parts).
//!
//! The visible text replaces the composer's text as one edit, so undo and
//! the draft's persistence see it as typing. The parts join the draft as
//! hidden context items (`DraftContext::hidden`), replacing any earlier
//! hand-off's: the composer does not list them, and the next send delivers
//! them as synthetic context with everything else the draft holds. A failed
//! send restores them with the draft.

use gpui_kit::{Context, EntityInputHandler, Window};

use crate::workspace::Workspace;

/// Where a hand-off goes.
pub(crate) enum HandoffTarget {
    /// The selected session's composer.
    CurrentSession,
    /// A new-session draft in this project.
    NewSession(String),
}

/// Why a hand-off could not be placed, as the locale key its notice shows.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HandoffRefusal {
    NoActiveSession,
    UnknownProject,
    Unavailable,
}

impl Workspace {
    /// Places `visible` in the target's composer with `parts` hidden beside
    /// it, then focuses the composer.
    pub(crate) fn hand_off_to_composer(
        &mut self,
        target: HandoffTarget,
        visible: String,
        parts: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), HandoffRefusal> {
        if !self.persistence_ready() || self.close_in_progress() {
            return Err(HandoffRefusal::Unavailable);
        }
        match target {
            HandoffTarget::CurrentSession => {
                if self.selected.is_none() {
                    return Err(HandoffRefusal::NoActiveSession);
                }
                if !self.composer_editable() {
                    return Err(HandoffRefusal::Unavailable);
                }
            }
            HandoffTarget::NewSession(directory) => {
                if self.draft_destination_project(&directory).is_none() {
                    return Err(HandoffRefusal::UnknownProject);
                }
                self.show_new_session(window, cx);
                self.choose_draft_project(&directory, window, cx);
            }
        }
        if !self.composer_editable() {
            return Err(HandoffRefusal::Unavailable);
        }
        self.composer.update(cx, |composer, cx| {
            let end = composer.value().len();
            composer.set_selected_range(0..end, cx);
            composer.replace_text_in_range(None, &visible, window, cx);
        });
        self.save_composer_draft(cx);
        if let Some(owner) = self.composer_key.clone()
            && let Some(mut draft) = self.drafts.get(&owner).cloned()
        {
            draft.content.replace_hidden(parts);
            self.drafts.save(owner, draft);
            self.queue_persistence(cx);
        }
        self.focus_composer(window, cx);
        cx.notify();
        Ok(())
    }
}
