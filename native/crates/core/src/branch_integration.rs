//! The branches the Git surface's Update branch dialog can merge or rebase
//! onto (`branchIntegration.ts` and the target list of
//! `BranchIntegrationSection.tsx`).
//!
//! A local branch is integrated by name. A remote-tracking branch is fetched
//! first, its one ref into its tracking ref, and then integrated at the
//! commit that fetch left there, so a concurrent fetch cannot change what
//! the merge or rebase takes.

use crate::search_rank::rank_by_query;

/// A branch to merge into the current one or to rebase onto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrationTarget {
    Local(String),
    Remote(RemoteTarget),
}

/// A remote-tracking branch and the refs its fetch moves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTarget {
    pub remote: String,
    pub branch: String,
    /// `<remote>/<branch>`, as the branch list shows it.
    pub label: String,
    /// `refs/heads/<branch>` on the remote.
    pub source_ref: String,
    /// `refs/remotes/<remote>/<branch>` here.
    pub destination_ref: String,
}

impl IntegrationTarget {
    /// The name the dialog shows.
    pub fn label(&self) -> &str {
        match self {
            Self::Local(branch) => branch,
            Self::Remote(target) => &target.label,
        }
    }
}

/// `createRemoteBranchIntegrationTargets`: the remote-tracking `labels` that
/// belong to one of `remote_names`, the longest matching remote name first
/// so `origin-fork/main` is not read as `origin`'s `fork/main`.
pub fn remote_targets(remote_names: &[String], labels: &[String]) -> Vec<RemoteTarget> {
    let mut names: Vec<&str> = remote_names.iter().map(String::as_str).collect();
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    let mut targets = Vec::new();
    for label in labels {
        let Some(remote) = names.iter().find(|name| {
            label
                .strip_prefix(**name)
                .is_some_and(|rest| rest.starts_with('/'))
        }) else {
            continue;
        };
        let branch = &label[remote.len() + 1..];
        if branch.is_empty() {
            continue;
        }
        targets.push(RemoteTarget {
            remote: (*remote).to_owned(),
            branch: branch.to_owned(),
            label: label.clone(),
            source_ref: format!("refs/heads/{branch}"),
            destination_ref: format!("refs/remotes/{remote}/{branch}"),
        });
    }
    targets
}

/// The local branches other than `current` and the remote targets that
/// match `query`, each ranked as `rankByQuery` ranks them.
pub fn filter_targets<'a>(
    local: &'a [String],
    remote: &'a [RemoteTarget],
    current: Option<&str>,
    query: &str,
) -> (Vec<&'a String>, Vec<&'a RemoteTarget>) {
    let candidates: Vec<&String> = local
        .iter()
        .filter(|branch| Some(branch.as_str()) != current)
        .collect();
    let local = rank_by_query(
        &candidates,
        query,
        |branch| vec![Some(branch.as_str())],
        true,
    )
    .into_iter()
    .map(|index| candidates[index])
    .collect();
    let remote = rank_by_query(
        remote,
        query,
        |target| {
            vec![
                Some(target.label.as_str()),
                Some(target.branch.as_str()),
                Some(target.remote.as_str()),
            ]
        },
        true,
    )
    .into_iter()
    .map(|index| &remote[index])
    .collect();
    (local, remote)
}

/// The target the dialog starts with (`updateTargetBranch`): the primary
/// remote's copy of the base branch when it is listed, else the local base
/// branch. The base is the remote's default branch; a branch is never its
/// own base, so none is chosen on the default branch itself.
pub fn default_target(
    primary_remote: Option<&str>,
    default_branch: Option<&str>,
    current: Option<&str>,
    local: &[String],
    remote: &[RemoteTarget],
) -> Option<IntegrationTarget> {
    let base = default_branch
        .map(str::trim)
        .filter(|base| !base.is_empty() && Some(*base) != current)?;
    if let Some(primary) = primary_remote {
        let label = format!("{primary}/{base}");
        if let Some(target) = remote.iter().find(|target| target.label == label) {
            return Some(IntegrationTarget::Remote(target.clone()));
        }
    }
    local
        .iter()
        .any(|branch| branch == base)
        .then(|| IntegrationTarget::Local(base.to_owned()))
}

/// Whether `value` is a full commit hash, as a fetched ref must resolve to.
pub fn is_commit_hash(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn remote_targets_take_the_longest_remote_name() {
        let targets = remote_targets(
            &strings(&["origin", "origin-fork"]),
            &strings(&[
                "origin/main",
                "origin-fork/feature/x",
                "upstream/main",
                "origin/",
            ]),
        );
        assert_eq!(
            targets.len(),
            2,
            "unknown remotes and empty branches are left out"
        );
        assert_eq!(targets[0].remote, "origin");
        assert_eq!(targets[0].destination_ref, "refs/remotes/origin/main");
        assert_eq!(targets[1].remote, "origin-fork");
        assert_eq!(targets[1].branch, "feature/x");
        assert_eq!(targets[1].source_ref, "refs/heads/feature/x");
    }

    #[test]
    fn filtering_leaves_out_the_current_branch_and_ranks_matches() {
        let local = strings(&["main", "feature/login", "fix/logout"]);
        let remote = remote_targets(&strings(&["origin"]), &strings(&["origin/main"]));
        let (found, remotes) = filter_targets(&local, &remote, Some("main"), "");
        assert_eq!(found, vec!["feature/login", "fix/logout"]);
        assert_eq!(remotes.len(), 1);
        let (found, remotes) = filter_targets(&local, &remote, Some("main"), "logout");
        assert_eq!(found, vec!["fix/logout"]);
        assert!(remotes.is_empty());
    }

    #[test]
    fn the_default_target_prefers_the_remote_copy_of_the_base() {
        let local = strings(&["main", "feature"]);
        let remote = remote_targets(&strings(&["origin"]), &strings(&["origin/main"]));
        assert_eq!(
            default_target(
                Some("origin"),
                Some("main"),
                Some("feature"),
                &local,
                &remote
            )
            .map(|target| target.label().to_owned()),
            Some("origin/main".to_owned())
        );
        assert_eq!(
            default_target(Some("origin"), Some("main"), Some("feature"), &local, &[]),
            Some(IntegrationTarget::Local("main".to_owned()))
        );
        assert_eq!(
            default_target(Some("origin"), Some("main"), Some("main"), &local, &remote),
            None,
            "the default branch is not its own base"
        );
        assert_eq!(
            default_target(None, None, Some("feature"), &local, &remote),
            None
        );
    }

    #[test]
    fn only_full_hashes_are_commits() {
        assert!(is_commit_hash(&"a".repeat(40)));
        assert!(is_commit_hash(&"0".repeat(64)));
        assert!(!is_commit_hash("abc123"));
        assert!(!is_commit_hash(&"g".repeat(40)));
    }
}
