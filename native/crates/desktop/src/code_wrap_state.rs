//! Ordering of one runtime's code-wrap writes and settings snapshots.
//!
//! Only one write may be in flight. Changes made during it replace the
//! queued value, which is sent after the current request settles. Read
//! responses cannot replace a choice made since the read began.

/// A captured code-wrap write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Save {
    revision: u64,
    pub(crate) value: bool,
}

/// One runtime's pending settings work. The view owns the displayed value.
#[derive(Debug, Default)]
pub(crate) struct CodeWrapState {
    revision: u64,
    pending: Option<bool>,
    in_flight: Option<Save>,
}

impl CodeWrapState {
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn change(&mut self, value: bool) {
        self.revision += 1;
        self.pending = Some(value);
    }

    pub(crate) fn begin_save(&mut self) -> Option<Save> {
        if self.in_flight.is_some() {
            return None;
        }
        let save = Save {
            revision: self.revision,
            value: self.pending.take()?,
        };
        self.in_flight = Some(save);
        Some(save)
    }

    /// Finishes a write, returning whether its response may update the view.
    pub(crate) fn finish_save(&mut self, save: Save) -> bool {
        if self.in_flight != Some(save) {
            return false;
        }
        self.in_flight = None;
        self.revision == save.revision
    }

    pub(crate) fn can_adopt(&self, read_revision: u64) -> bool {
        read_revision == self.revision && self.pending.is_none() && self.in_flight.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_serial_and_keep_the_last_queued_choice() {
        let mut state = CodeWrapState::default();
        state.change(false);
        let first = state.begin_save().expect("first change is queued");
        state.change(true);
        state.change(false);
        state.change(true);
        assert!(state.begin_save().is_none(), "only one request may run");
        assert!(
            !state.finish_save(first),
            "newer choices win over old responses"
        );
        let last = state.begin_save().expect("the newest choice follows");
        assert!(last.value);
        assert!(state.finish_save(last));
        assert!(
            state.begin_save().is_none(),
            "intermediate choices are coalesced"
        );
    }

    #[test]
    fn snapshots_wait_for_writes_and_cannot_undo_newer_choices() {
        let mut state = CodeWrapState::default();
        let old_read = state.revision();
        assert!(state.can_adopt(old_read));
        state.change(false);
        assert!(!state.can_adopt(old_read));
        assert!(
            !state.can_adopt(state.revision()),
            "pending writes take precedence"
        );
        let save = state.begin_save().expect("change is queued");
        assert!(
            !state.can_adopt(state.revision()),
            "writes in flight take precedence"
        );
        assert!(state.finish_save(save));
        assert!(
            !state.can_adopt(old_read),
            "even a completed write invalidates the old read"
        );
        assert!(state.can_adopt(state.revision()));
    }

    #[test]
    fn failed_request_releases_the_queue_without_retrying_it() {
        let mut state = CodeWrapState::default();
        state.change(false);
        let failed = state.begin_save().expect("change is queued");
        state.change(true);
        assert!(!state.finish_save(failed));
        let next = state.begin_save().expect("newer choice survives failure");
        assert!(next.value);
        assert!(state.finish_save(next));
        assert!(
            !state.finish_save(failed),
            "duplicate completion is ignored"
        );
        assert!(state.begin_save().is_none());
    }
}
