//! Explicit loading states that keep failures distinct from empty results.

/// The state of a server-backed collection.
///
/// A failed load keeps the last snapshot, so a fetch failure never looks like
/// an authoritative empty list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LoadState<T> {
    #[default]
    Absent,
    Loading {
        retained: Option<T>,
    },
    /// Some pages arrived; the snapshot carries no full-list authority.
    Partial(T),
    Complete(T),
    Failed {
        retained: Option<T>,
        error: String,
    },
}

impl<T> LoadState<T> {
    /// The newest data, whatever the state.
    pub fn data(&self) -> Option<&T> {
        match self {
            Self::Absent => None,
            Self::Loading { retained } | Self::Failed { retained, .. } => retained.as_ref(),
            Self::Partial(data) | Self::Complete(data) => Some(data),
        }
    }

    /// Whether the data covers the whole scope, so cleanup may rely on it.
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete(_))
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }

    /// Starts a load, keeping whatever data exists.
    pub fn begin_loading(self) -> Self {
        Self::Loading {
            retained: self.into_data(),
        }
    }

    /// Records a failure without discarding earlier data.
    pub fn fail(self, error: impl Into<String>) -> Self {
        Self::Failed {
            retained: self.into_data(),
            error: error.into(),
        }
    }

    pub fn into_data(self) -> Option<T> {
        match self {
            Self::Absent => None,
            Self::Loading { retained } | Self::Failed { retained, .. } => retained,
            Self::Partial(data) | Self::Complete(data) => Some(data),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_retains_the_previous_snapshot() {
        let state = LoadState::Complete(vec![1, 2])
            .begin_loading()
            .fail("offline");
        assert_eq!(state.data(), Some(&vec![1, 2]));
        assert!(!state.is_complete());
    }

    #[test]
    fn an_empty_complete_result_is_authoritative() {
        let state: LoadState<Vec<u8>> = LoadState::Complete(Vec::new());
        assert!(state.is_complete());
        assert_eq!(state.data().map(Vec::len), Some(0));
    }
}
