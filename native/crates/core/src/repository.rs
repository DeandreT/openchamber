//! What the work status card reports about a session's repository, from the
//! server's Git status (`WorkStatusPrimaryGroup.tsx`).

/// An operation Git is in the middle of, which needs the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Attention {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl Attention {
    /// The status's `attentionReason`, as the server names it.
    pub fn from_reason(reason: &str) -> Option<Self> {
        match reason {
            "merge" => Some(Self::Merge),
            "rebase" => Some(Self::Rebase),
            "cherry-pick" => Some(Self::CherryPick),
            "revert" => Some(Self::Revert),
            "bisect" => Some(Self::Bisect),
            _ => None,
        }
    }
}

/// The working tree's changes: the changed files and, when the server
/// counted them, the lines added and removed across staged and unstaged
/// edits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub files: usize,
    pub line_counts: Option<LineCounts>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineCounts {
    pub additions: u64,
    pub deletions: u64,
}

/// A repository's status as the card reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryStatus {
    /// The checked-out branch, trimmed; `None` when detached or unnamed.
    pub branch: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    pub changes: Changes,
    pub attention: Option<Attention>,
}

impl RepositoryStatus {
    /// The changes row: shown only when files changed.
    pub fn changed(&self) -> Option<Changes> {
        (self.changes.files > 0).then_some(self.changes)
    }

    /// The ahead and behind counts the branch row shows, `↑N` and `↓N`,
    /// leaving out zeros.
    pub fn divergence(&self) -> Vec<String> {
        let mut counts = Vec::new();
        if self.ahead > 0 {
            counts.push(format!("↑{}", self.ahead));
        }
        if self.behind > 0 {
            counts.push(format!("↓{}", self.behind));
        }
        counts
    }
}

/// A Git status read: a repository's status, or no repository there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitStatusRead {
    Repository(RepositoryStatus),
    NotRepository,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divergence_leaves_out_zeros() {
        let status = |ahead, behind| RepositoryStatus {
            ahead,
            behind,
            ..RepositoryStatus::default()
        };
        assert!(status(0, 0).divergence().is_empty());
        assert_eq!(status(2, 0).divergence(), ["↑2"]);
        assert_eq!(status(1, 3).divergence(), ["↑1", "↓3"]);
    }

    #[test]
    fn a_clean_tree_has_no_changes_row() {
        assert_eq!(RepositoryStatus::default().changed(), None);
        let dirty = RepositoryStatus {
            changes: Changes {
                files: 2,
                line_counts: None,
            },
            ..RepositoryStatus::default()
        };
        assert_eq!(dirty.changed().map(|changes| changes.files), Some(2));
    }

    #[test]
    fn attention_reasons_parse_by_name() {
        assert_eq!(
            Attention::from_reason("cherry-pick"),
            Some(Attention::CherryPick)
        );
        assert_eq!(Attention::from_reason("unknown"), None);
    }
}
