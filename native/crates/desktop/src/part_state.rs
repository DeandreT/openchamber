//! What the reader opened in one session's transcript.
//!
//! Tool rows start folded and open on a click. Reasoning and compaction
//! blocks follow their default until the reader toggles one; the choice then
//! stays for the session, as `ReasoningTimelineBlock`'s user expansion does.
//! A tool row that opens on a result a history page left out reads its whole
//! message; the read's progress is kept per message so every elided call of
//! that message shares it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use std::sync::Arc;

use gpui_kit::{Image, SharedString, Task};
use openchamber_native_core::ids::{MessageId, PartId};
use openchamber_native_core::tool_shells;

/// How long a whole-message read runs before its row says so
/// (`useToolContentCompletion`'s progress delay).
pub(crate) const PROGRESS_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// A whole-message read for elided results.
pub(crate) enum Hydration {
    Loading {
        _read: Task<()>,
        _delay: Task<()>,
        /// The read outlasted the progress delay.
        slow: bool,
    },
    Failed,
}

/// How a row draws a whole-message read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HydrationView {
    Loading { slow: bool },
    Failed,
}

#[derive(Default)]
pub(crate) struct PartState {
    expanded: HashSet<PartId>,
    blocks: HashMap<PartId, bool>,
    hydration: HashMap<MessageId, Hydration>,
    /// Folded reasoning previews by part, with the text length they were
    /// made from. Reasoning only grows, so a length change means new text.
    summaries: RefCell<HashMap<PartId, (usize, SharedString)>>,
    /// Attachment images by part, decoded once; `None` when one cannot be.
    images: RefCell<HashMap<PartId, Option<Arc<Image>>>>,
}

impl PartState {
    pub(crate) fn is_expanded(&self, part: &PartId) -> bool {
        self.expanded.contains(part)
    }

    /// Opens or folds a tool row; answers whether it is now open.
    pub(crate) fn toggle_tool(&mut self, part: &PartId) -> bool {
        if self.expanded.remove(part) {
            return false;
        }
        self.expanded.insert(part.clone());
        true
    }

    /// Whether a block shows its body: the reader's choice, else `default`.
    pub(crate) fn block_open(&self, part: &PartId, default: bool) -> bool {
        self.blocks.get(part).copied().unwrap_or(default)
    }

    pub(crate) fn toggle_block(&mut self, part: &PartId, default: bool) {
        let open = self.block_open(part, default);
        self.blocks.insert(part.clone(), !open);
    }

    pub(crate) fn hydration(&self, message: &MessageId) -> Option<HydrationView> {
        self.hydration.get(message).map(|state| match state {
            Hydration::Loading { slow, .. } => HydrationView::Loading { slow: *slow },
            Hydration::Failed => HydrationView::Failed,
        })
    }

    /// Whether a whole read of `message` should start: none runs, and the
    /// last one did not fail (a failure waits for Retry).
    pub(crate) fn needs_hydration(&self, message: &MessageId) -> bool {
        !self.hydration.contains_key(message)
    }

    pub(crate) fn start_hydration(&mut self, message: MessageId, read: Task<()>, delay: Task<()>) {
        self.hydration.insert(
            message,
            Hydration::Loading {
                _read: read,
                _delay: delay,
                slow: false,
            },
        );
    }

    /// Marks a running read as slow; answers whether that changed anything.
    pub(crate) fn mark_slow(&mut self, message: &MessageId) -> bool {
        match self.hydration.get_mut(message) {
            Some(Hydration::Loading { slow, .. }) if !*slow => {
                *slow = true;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn finish_hydration(&mut self, message: &MessageId, succeeded: bool) {
        if succeeded {
            self.hydration.remove(message);
        } else {
            self.hydration.insert(message.clone(), Hydration::Failed);
        }
    }

    /// Clears a failure so the next open reads again.
    pub(crate) fn clear_failure(&mut self, message: &MessageId) {
        if matches!(self.hydration.get(message), Some(Hydration::Failed)) {
            self.hydration.remove(message);
        }
    }

    /// An attachment's image, made by `decode` the first time it is drawn.
    pub(crate) fn attachment_image(
        &self,
        part: &PartId,
        decode: impl FnOnce() -> Option<Arc<Image>>,
    ) -> Option<Arc<Image>> {
        self.images
            .borrow_mut()
            .entry(part.clone())
            .or_insert_with(decode)
            .clone()
    }

    /// The folded preview of a reasoning part, made once per text length.
    pub(crate) fn reasoning_summary(&self, part: &PartId, text: &str) -> SharedString {
        let mut summaries = self.summaries.borrow_mut();
        if let Some((length, summary)) = summaries.get(part)
            && *length == text.len()
        {
            return summary.clone();
        }
        let summary: SharedString = tool_shells::reasoning_summary(text).into();
        summaries.insert(part.clone(), (text.len(), summary.clone()));
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_rows_toggle_and_blocks_keep_the_readers_choice() {
        let mut state = PartState::default();
        let part = PartId::from("call_1");
        assert!(!state.is_expanded(&part));
        assert!(state.toggle_tool(&part));
        assert!(state.is_expanded(&part));
        assert!(!state.toggle_tool(&part));

        let block = PartId::from("reasoning_1");
        assert!(!state.block_open(&block, false));
        state.toggle_block(&block, false);
        assert!(state.block_open(&block, false));
        assert!(
            state.block_open(&block, false),
            "a streaming default changing later does not undo the choice"
        );
        state.toggle_block(&block, true);
        assert!(!state.block_open(&block, true));
    }

    #[test]
    fn a_failed_read_waits_for_retry() {
        let mut state = PartState::default();
        let message = MessageId::from("m1");
        assert!(state.needs_hydration(&message));
        state.start_hydration(message.clone(), Task::ready(()), Task::ready(()));
        assert_eq!(
            state.hydration(&message),
            Some(HydrationView::Loading { slow: false })
        );
        assert!(state.mark_slow(&message));
        assert!(!state.mark_slow(&message));
        assert_eq!(
            state.hydration(&message),
            Some(HydrationView::Loading { slow: true })
        );
        assert!(!state.needs_hydration(&message));
        state.finish_hydration(&message, false);
        assert_eq!(state.hydration(&message), Some(HydrationView::Failed));
        assert!(
            !state.needs_hydration(&message),
            "a failure is not retried on its own"
        );
        state.clear_failure(&message);
        assert!(state.needs_hydration(&message));
        state.start_hydration(message.clone(), Task::ready(()), Task::ready(()));
        state.finish_hydration(&message, true);
        assert_eq!(state.hydration(&message), None);
    }

    #[test]
    fn summaries_are_made_once_per_text_length() {
        let state = PartState::default();
        let part = PartId::from("r");
        let first = state.reasoning_summary(&part, "**Plan** the work");
        assert_eq!(first.as_ref(), "Plan the work");
        // Text of the same length is the text already summarized: reasoning
        // only grows, so the cache does not compare the text again.
        let again = state.reasoning_summary(&part, "**Plan** the wXrk");
        assert_eq!(
            again.as_ref(),
            "Plan the work",
            "the cached preview is reused"
        );
        assert_eq!(
            state
                .reasoning_summary(&part, "**Plan** the work now")
                .as_ref(),
            "Plan the work now"
        );
    }
}
