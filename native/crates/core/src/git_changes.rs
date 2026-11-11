//! The Git panel's model: a repository's status split into staged and
//! unstaged changes, the order of index mutations and status reads, and the
//! planned network operations that push, pull and fetch.
//!
//! Ports the change scopes of `views/git/ChangesPanel.tsx`, the queue of
//! `views/git/gitIndexMutationQueue.ts` and the request building and result
//! checks of `lib/boundGitNetworkOperation.ts`. The status the server answers
//! is the authority: nothing here shows a mutation before the status read
//! after it.

use std::collections::{HashMap, VecDeque};

/// What changed about one path, from its porcelain status letters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

impl ChangeKind {
    fn from_letter(letter: char) -> Option<Self> {
        match letter {
            'A' => Some(Self::Added),
            'M' => Some(Self::Modified),
            'D' => Some(Self::Deleted),
            'R' => Some(Self::Renamed),
            'C' => Some(Self::Copied),
            'T' => Some(Self::TypeChanged),
            '?' => Some(Self::Untracked),
            'U' => Some(Self::Conflicted),
            _ => None,
        }
    }

    /// The one-letter badge a change row shows.
    pub fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::Copied => 'C',
            Self::TypeChanged => 'T',
            Self::Untracked => 'U',
            Self::Conflicted => '!',
        }
    }
}

/// One path of the status with its index and worktree letters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStatus {
    pub path: String,
    pub index: char,
    pub working: char,
}

impl FileStatus {
    /// Both sides changed in a merge: `DD`, `AU`, `UD`, `UA`, `DU`, `AA`,
    /// `UU`.
    pub fn is_conflicted(&self) -> bool {
        matches!(
            (self.index, self.working),
            ('D', 'D')
                | ('A', 'U')
                | ('U', 'D')
                | ('U', 'A')
                | ('D', 'U')
                | ('A', 'A')
                | ('U', 'U')
        )
    }

    fn staged_kind(&self) -> Option<ChangeKind> {
        if self.is_conflicted() || self.index == '?' {
            return None;
        }
        ChangeKind::from_letter(self.index)
    }

    fn unstaged_kind(&self) -> Option<ChangeKind> {
        if self.is_conflicted() {
            return Some(ChangeKind::Conflicted);
        }
        ChangeKind::from_letter(self.working)
    }
}

/// Lines added and removed in one path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineStat {
    pub insertions: u64,
    pub deletions: u64,
}

/// A repository's status as the Git panel reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitChanges {
    /// The checked-out branch; `None` when detached.
    pub branch: Option<String>,
    /// `remote/branch` the branch tracks.
    pub tracking: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    /// No commit exists yet.
    pub unborn: bool,
    pub files: Vec<FileStatus>,
    pub staged_stats: HashMap<String, LineStat>,
    pub working_stats: HashMap<String, LineStat>,
    pub merge: Option<MergeState>,
    pub rebase: Option<RebaseState>,
}

/// A merge the repository is in the middle of (`mergeInProgress`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeState {
    pub head: String,
    pub message: Option<String>,
}

/// A rebase the repository is in the middle of (`rebaseInProgress`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RebaseState {
    pub head_name: Option<String>,
    pub onto: Option<String>,
}

/// An integration that can stop on conflicts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Integration {
    Merge,
    Rebase,
}

/// What a status read found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangesRead {
    NotRepository,
    Repository(Box<GitChanges>),
}

/// The scope a change row belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeScope {
    Staged,
    Unstaged,
}

/// One row of the Changes list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeRow {
    pub path: String,
    pub kind: ChangeKind,
    pub scope: ChangeScope,
    pub stat: Option<LineStat>,
}

impl GitChanges {
    /// Rows whose index side changed, in path order.
    pub fn staged(&self) -> Vec<ChangeRow> {
        self.rows(ChangeScope::Staged)
    }

    /// Rows whose worktree side changed or that are untracked or conflicted.
    pub fn unstaged(&self) -> Vec<ChangeRow> {
        self.rows(ChangeScope::Unstaged)
    }

    pub fn is_clean(&self) -> bool {
        self.files.is_empty()
    }

    /// The operation in progress, as the banner shows it: a merge with a
    /// head, else a rebase naming its branch or base.
    pub fn in_progress(&self) -> Option<Integration> {
        if self
            .merge
            .as_ref()
            .is_some_and(|merge| !merge.head.is_empty())
        {
            return Some(Integration::Merge);
        }
        self.rebase
            .as_ref()
            .filter(|rebase| rebase.head_name.is_some() || rebase.onto.is_some())
            .map(|_| Integration::Rebase)
    }

    /// Paths both sides changed, in path order (`isConflictedStatusFile`).
    pub fn conflicted(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .files
            .iter()
            .filter(|file| file.is_conflicted())
            .map(|file| file.path.clone())
            .collect();
        paths.sort();
        paths
    }

    fn rows(&self, scope: ChangeScope) -> Vec<ChangeRow> {
        let mut rows: Vec<ChangeRow> = self
            .files
            .iter()
            .filter_map(|file| {
                let (kind, stats) = match scope {
                    ChangeScope::Staged => (file.staged_kind()?, &self.staged_stats),
                    ChangeScope::Unstaged => (file.unstaged_kind()?, &self.working_stats),
                };
                Some(ChangeRow {
                    path: file.path.clone(),
                    kind,
                    scope,
                    stat: stats.get(&file.path).copied(),
                })
            })
            .collect();
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        rows
    }
}

/// A status read the caller performs and hands back to
/// [`StatusLoads::complete`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a ticket names a read the caller must perform"]
pub struct StatusTicket(u64);

/// The order of status reads: only the newest read applies, so a slow read
/// that started before a later one, or before a mutation finished, never
/// replaces what that later read shows.
#[derive(Debug, Default)]
pub struct StatusLoads {
    next: u64,
    /// The newest read started.
    newest: u64,
    current: Option<ChangesRead>,
    /// The last read failed; `current` stays.
    failed: bool,
}

impl StatusLoads {
    pub fn begin(&mut self) -> StatusTicket {
        self.next += 1;
        self.newest = self.next;
        StatusTicket(self.next)
    }

    /// Applies a finished read; `Err` keeps the last status. Returns whether
    /// the read was the newest.
    pub fn complete(&mut self, ticket: StatusTicket, read: Result<ChangesRead, ()>) -> bool {
        if ticket.0 != self.newest {
            return false;
        }
        match read {
            Ok(read) => {
                self.current = Some(read);
                self.failed = false;
            }
            Err(()) => self.failed = true,
        }
        true
    }

    pub fn current(&self) -> Option<&ChangesRead> {
        self.current.as_ref()
    }

    pub fn repository(&self) -> Option<&GitChanges> {
        match &self.current {
            Some(ChangesRead::Repository(changes)) => Some(changes.as_ref()),
            _ => None,
        }
    }

    /// The last read failed, whatever is still shown.
    pub fn failed(&self) -> bool {
        self.failed
    }
}

/// A change to the index or worktree the user asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexMutation {
    Stage(Vec<String>),
    Unstage(Vec<String>),
    /// Discards the worktree's changes of one path.
    Discard(String),
}

/// Index mutations in the order the user asked for them, one at a time
/// (`gitIndexMutationQueue.ts`). Each finished mutation is followed by a
/// status read; a failed one shows its error and still reads, so no row keeps
/// a state the server did not report.
#[derive(Debug, Default)]
pub struct IndexQueue {
    pending: VecDeque<IndexMutation>,
    running: Option<IndexMutation>,
}

impl IndexQueue {
    /// Queues `mutation`; returns it when nothing runs, for the caller to
    /// start now.
    pub fn push(&mut self, mutation: IndexMutation) -> Option<IndexMutation> {
        if self.running.is_none() {
            self.running = Some(mutation.clone());
            return Some(mutation);
        }
        self.pending.push_back(mutation);
        None
    }

    /// The running mutation finished; returns the next one to start.
    pub fn finished(&mut self) -> Option<IndexMutation> {
        self.running = self.pending.pop_front();
        self.running.clone()
    }

    pub fn is_busy(&self) -> bool {
        self.running.is_some()
    }

    /// Whether `path` is part of a running or queued mutation.
    pub fn touches(&self, path: &str) -> bool {
        self.running
            .iter()
            .chain(&self.pending)
            .any(|mutation| match mutation {
                IndexMutation::Stage(paths) | IndexMutation::Unstage(paths) => {
                    paths.iter().any(|candidate| candidate == path)
                }
                IndexMutation::Discard(candidate) => candidate == path,
            })
    }
}

/// A remote endpoint as the server redacts it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEndpoint {
    pub display_url: String,
    pub fingerprint: String,
}

/// How the server's Git reaches a remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportMode {
    System,
    Managed,
    Anonymous,
}

impl TransportMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Managed => "managed",
            Self::Anonymous => "anonymous",
        }
    }
}

/// One remote with the grant a network operation runs under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteGrant {
    pub name: String,
    pub fetch: RemoteEndpoint,
    pub push: RemoteEndpoint,
    pub mode: TransportMode,
    /// The grant still matches the repository's configured endpoints.
    pub ready: bool,
}

/// The repository's transport authority (`effectiveRepositoryBinding`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryBinding {
    pub repository_id: String,
    pub revision: u64,
    pub config_revision: String,
    pub remotes: Vec<RemoteGrant>,
}

/// A network operation the user asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NetworkAction {
    Fetch,
    Pull,
    Push,
}

/// A complete plan request for the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkRequest {
    pub action: NetworkAction,
    pub directory: String,
    pub repository_id: String,
    pub binding_revision: u64,
    pub config_revision: String,
    pub remote: String,
    pub endpoint: RemoteEndpoint,
    /// `None` for a whole-remote fetch.
    pub source_ref: Option<String>,
    pub destination_ref: Option<String>,
    pub transport: TransportMode,
}

/// Why an operation cannot be planned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkRefusal {
    /// The checkout has no branch, as on a detached HEAD.
    BranchRequired,
    /// The branch tracks nothing to pull from or push to.
    TrackingRequired,
    /// No remote is configured.
    NoRemote,
    /// The tracked remote is not in the binding.
    RemoteMissing,
    /// The remote's grant no longer matches its endpoints.
    NeedsAttention,
    /// The remote reads anonymously and cannot take a push.
    AnonymousReadOnly,
}

/// Builds the plan request for `action`, as `buildBoundGitNetworkOperationRequest`
/// and `buildBoundBranchPushRequest` do for a branch that tracks its remote.
/// A fetch without tracking fetches the only or first remote; pull and push
/// need tracking.
pub fn network_request(
    action: NetworkAction,
    directory: &str,
    binding: &RepositoryBinding,
    changes: &GitChanges,
) -> Result<NetworkRequest, NetworkRefusal> {
    let tracking = changes
        .tracking
        .as_deref()
        .map(str::trim)
        .filter(|tracking| !tracking.is_empty());
    let branch = changes
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty() && *branch != "HEAD");
    let tracked = tracking.and_then(|tracking| {
        binding
            .remotes
            .iter()
            .filter(|remote| {
                tracking
                    .strip_prefix(remote.name.as_str())
                    .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1)
            })
            .max_by_key(|remote| remote.name.len())
            .map(|remote| (remote, &tracking[remote.name.len() + 1..]))
    });
    let request = |remote: &RemoteGrant,
                   endpoint: &RemoteEndpoint,
                   source: Option<String>,
                   destination: Option<String>| NetworkRequest {
        action,
        directory: directory.to_owned(),
        repository_id: binding.repository_id.clone(),
        binding_revision: binding.revision,
        config_revision: binding.config_revision.clone(),
        remote: remote.name.clone(),
        endpoint: endpoint.clone(),
        source_ref: source,
        destination_ref: destination,
        transport: remote.mode,
    };
    let ready = |remote: &RemoteGrant| {
        if remote.ready {
            Ok(())
        } else {
            Err(NetworkRefusal::NeedsAttention)
        }
    };
    match action {
        NetworkAction::Fetch => {
            let remote = match tracked {
                Some((remote, _)) => remote,
                None if tracking.is_some() => return Err(NetworkRefusal::RemoteMissing),
                None => binding.remotes.first().ok_or(NetworkRefusal::NoRemote)?,
            };
            ready(remote)?;
            Ok(request(remote, &remote.fetch, None, None))
        }
        NetworkAction::Pull | NetworkAction::Push => {
            let branch = branch.ok_or(NetworkRefusal::BranchRequired)?;
            if binding.remotes.is_empty() {
                return Err(NetworkRefusal::NoRemote);
            }
            let Some((remote, tracked_branch)) = tracked else {
                return Err(if tracking.is_some() {
                    NetworkRefusal::RemoteMissing
                } else {
                    NetworkRefusal::TrackingRequired
                });
            };
            ready(remote)?;
            if action == NetworkAction::Pull {
                return Ok(request(
                    remote,
                    &remote.fetch,
                    Some(format!("refs/heads/{tracked_branch}")),
                    Some(format!("refs/heads/{branch}")),
                ));
            }
            if remote.mode == TransportMode::Anonymous {
                return Err(NetworkRefusal::AnonymousReadOnly);
            }
            Ok(request(
                remote,
                &remote.push,
                Some(format!("refs/heads/{branch}")),
                Some(format!("refs/heads/{tracked_branch}")),
            ))
        }
    }
}

/// A fetch of one remote branch into its tracking ref, as the Update branch
/// dialog runs before integrating a remote-tracking branch
/// (`buildBoundGitNetworkOperationRequest` with a `fetchTarget`).
pub fn ref_fetch_request(
    directory: &str,
    binding: &RepositoryBinding,
    remote_name: &str,
    branch: &str,
) -> Result<NetworkRequest, NetworkRefusal> {
    if branch.trim().is_empty() || branch == "HEAD" {
        return Err(NetworkRefusal::BranchRequired);
    }
    let remote = binding
        .remotes
        .iter()
        .find(|remote| remote.name == remote_name)
        .ok_or(NetworkRefusal::RemoteMissing)?;
    if !remote.ready {
        return Err(NetworkRefusal::NeedsAttention);
    }
    Ok(NetworkRequest {
        action: NetworkAction::Fetch,
        directory: directory.to_owned(),
        repository_id: binding.repository_id.clone(),
        binding_revision: binding.revision,
        config_revision: binding.config_revision.clone(),
        remote: remote.name.clone(),
        endpoint: remote.fetch.clone(),
        source_ref: Some(format!("refs/heads/{branch}")),
        destination_ref: Some(format!("refs/remotes/{remote_name}/{branch}")),
        transport: remote.mode,
    })
}

/// Where a network operation stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OperationState {
    Planned,
    Running,
    Succeeded,
    Partial,
    Conflicted,
    Failed,
    Cancelled,
    /// The server cannot tell whether the remote took the update.
    OutcomeUnknown,
}

impl OperationState {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "planned" => Self::Planned,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "partial" => Self::Partial,
            "conflicted" => Self::Conflicted,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "outcome-unknown" => Self::OutcomeUnknown,
            _ => return None,
        })
    }

    /// The wire name, also the suffix of its `gitView.operation.state.*` key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Partial => "partial",
            Self::Conflicted => "conflicted",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::OutcomeUnknown => "outcome-unknown",
        }
    }

    /// Whether the operation may still change.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Planned | Self::Running)
    }
}

/// A server snapshot of one network operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkOperation {
    pub id: String,
    pub runtime_id: String,
    pub action: NetworkAction,
    pub state: OperationState,
    pub completed_steps: Vec<String>,
    /// The server's stable error code and redacted message, when it failed.
    pub error: Option<OperationError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationError {
    pub code: String,
    pub message: String,
}

/// Why a later snapshot cannot describe the same operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotRejection {
    /// Another operation, another runtime or another action.
    Mismatch,
    /// A finished operation changed its outcome, or steps disappeared.
    InvalidTransition,
}

/// Accepts `next` as the newer snapshot of `held` (`acceptOperation`).
pub fn accept_snapshot(
    held: &NetworkOperation,
    next: NetworkOperation,
) -> Result<NetworkOperation, SnapshotRejection> {
    if next.id != held.id || next.runtime_id != held.runtime_id || next.action != held.action {
        return Err(SnapshotRejection::Mismatch);
    }
    if !held.state.is_open() && next.state != held.state {
        return Err(SnapshotRejection::InvalidTransition);
    }
    if held
        .completed_steps
        .iter()
        .any(|step| !next.completed_steps.contains(step))
    {
        return Err(SnapshotRejection::InvalidTransition);
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, index: char, working: char) -> FileStatus {
        FileStatus {
            path: path.to_owned(),
            index,
            working,
        }
    }

    fn paths(rows: &[ChangeRow]) -> Vec<(&str, char)> {
        rows.iter()
            .map(|row| (row.path.as_str(), row.kind.letter()))
            .collect()
    }

    #[test]
    fn a_partly_staged_file_has_a_row_in_each_scope() {
        let mut changes = GitChanges {
            files: vec![
                file("src/b.txt", 'M', 'M'),
                file("a.txt", 'A', ' '),
                file("notes/", '?', '?'),
                file("gone.txt", ' ', 'D'),
                file("both.txt", 'U', 'U'),
            ],
            ..GitChanges::default()
        };
        changes.staged_stats.insert(
            "src/b.txt".to_owned(),
            LineStat {
                insertions: 1,
                deletions: 0,
            },
        );
        changes.working_stats.insert(
            "src/b.txt".to_owned(),
            LineStat {
                insertions: 0,
                deletions: 2,
            },
        );
        let staged = changes.staged();
        let unstaged = changes.unstaged();
        assert_eq!(paths(&staged), [("a.txt", 'A'), ("src/b.txt", 'M')]);
        assert_eq!(
            paths(&unstaged),
            [
                ("both.txt", '!'),
                ("gone.txt", 'D'),
                ("notes/", 'U'),
                ("src/b.txt", 'M')
            ]
        );
        assert_eq!(staged[1].stat.map(|stat| stat.insertions), Some(1));
        assert_eq!(unstaged[3].stat.map(|stat| stat.deletions), Some(2));
    }

    #[test]
    fn only_the_newest_status_read_applies_and_failure_keeps_the_last() {
        let mut loads = StatusLoads::default();
        let first = loads.begin();
        let second = loads.begin();
        assert!(loads.complete(second, Ok(ChangesRead::NotRepository)));
        assert!(
            !loads.complete(first, Ok(ChangesRead::Repository(Box::default()))),
            "an older read is dropped"
        );
        assert_eq!(loads.current(), Some(&ChangesRead::NotRepository));
        let third = loads.begin();
        assert!(loads.complete(third, Err(())));
        assert!(loads.failed());
        assert_eq!(loads.current(), Some(&ChangesRead::NotRepository));
    }

    #[test]
    fn index_mutations_run_one_at_a_time_in_order() {
        let mut queue = IndexQueue::default();
        let stage = IndexMutation::Stage(vec!["a".to_owned()]);
        assert_eq!(queue.push(stage.clone()), Some(stage));
        assert_eq!(queue.push(IndexMutation::Discard("b".to_owned())), None);
        assert_eq!(
            queue.push(IndexMutation::Unstage(vec!["a".to_owned()])),
            None
        );
        assert!(queue.touches("b"));
        assert_eq!(
            queue.finished(),
            Some(IndexMutation::Discard("b".to_owned()))
        );
        assert_eq!(
            queue.finished(),
            Some(IndexMutation::Unstage(vec!["a".to_owned()]))
        );
        assert_eq!(queue.finished(), None);
        assert!(!queue.is_busy());
    }

    fn endpoint(name: &str) -> RemoteEndpoint {
        RemoteEndpoint {
            display_url: format!("https://host/{name}.git"),
            fingerprint: name.to_owned(),
        }
    }

    fn binding(remotes: &[(&str, TransportMode, bool)]) -> RepositoryBinding {
        RepositoryBinding {
            repository_id: "repo_1".to_owned(),
            revision: 3,
            config_revision: "cfg".to_owned(),
            remotes: remotes
                .iter()
                .map(|(name, mode, ready)| RemoteGrant {
                    name: (*name).to_owned(),
                    fetch: endpoint(&format!("{name}-fetch")),
                    push: endpoint(&format!("{name}-push")),
                    mode: *mode,
                    ready: *ready,
                })
                .collect(),
        }
    }

    fn tracking(branch: Option<&str>, tracking: Option<&str>) -> GitChanges {
        GitChanges {
            branch: branch.map(str::to_owned),
            tracking: tracking.map(str::to_owned),
            ..GitChanges::default()
        }
    }

    #[test]
    fn push_and_pull_use_the_tracked_remote_and_branch() {
        let binding = binding(&[
            ("origin", TransportMode::System, true),
            ("origin/fork", TransportMode::System, true),
        ]);
        let status = tracking(Some("feature"), Some("origin/fork/main"));
        let push = network_request(NetworkAction::Push, "/p", &binding, &status).expect("push");
        assert_eq!(push.remote, "origin/fork", "the longest matching remote");
        assert_eq!(push.endpoint.fingerprint, "origin/fork-push");
        assert_eq!(push.source_ref.as_deref(), Some("refs/heads/feature"));
        assert_eq!(push.destination_ref.as_deref(), Some("refs/heads/main"));
        assert_eq!(
            (push.binding_revision, push.config_revision.as_str()),
            (3, "cfg")
        );
        let pull = network_request(NetworkAction::Pull, "/p", &binding, &status).expect("pull");
        assert_eq!(pull.endpoint.fingerprint, "origin/fork-fetch");
        assert_eq!(pull.source_ref.as_deref(), Some("refs/heads/main"));
        assert_eq!(pull.destination_ref.as_deref(), Some("refs/heads/feature"));
    }

    #[test]
    fn fetch_reads_a_whole_remote() {
        let binding = binding(&[("origin", TransportMode::Anonymous, true)]);
        let fetch = network_request(NetworkAction::Fetch, "/p", &binding, &tracking(None, None))
            .expect("a detached checkout still fetches");
        assert_eq!((fetch.source_ref, fetch.destination_ref), (None, None));
        assert_eq!(fetch.transport, TransportMode::Anonymous);
    }

    #[test]
    fn a_ref_fetch_moves_one_branch_into_its_tracking_ref() {
        let ready = binding(&[
            ("origin", TransportMode::System, true),
            ("fork", TransportMode::System, false),
        ]);
        let fetch = ref_fetch_request("/p", &ready, "origin", "feature/x")
            .expect("a ready remote fetches one branch");
        assert_eq!(fetch.endpoint, endpoint("origin-fetch"));
        assert_eq!(fetch.source_ref.as_deref(), Some("refs/heads/feature/x"));
        assert_eq!(
            fetch.destination_ref.as_deref(),
            Some("refs/remotes/origin/feature/x")
        );
        assert_eq!(
            ref_fetch_request("/p", &ready, "fork", "main").err(),
            Some(NetworkRefusal::NeedsAttention)
        );
        assert_eq!(
            ref_fetch_request("/p", &ready, "upstream", "main").err(),
            Some(NetworkRefusal::RemoteMissing)
        );
        assert_eq!(
            ref_fetch_request("/p", &ready, "origin", "").err(),
            Some(NetworkRefusal::BranchRequired)
        );
    }

    #[test]
    fn refusals_name_what_is_missing() {
        let ready = binding(&[("origin", TransportMode::System, true)]);
        let refuse = |action, binding: &RepositoryBinding, status: &GitChanges| {
            network_request(action, "/p", binding, status).err()
        };
        assert_eq!(
            refuse(
                NetworkAction::Push,
                &ready,
                &tracking(None, Some("origin/main"))
            ),
            Some(NetworkRefusal::BranchRequired)
        );
        assert_eq!(
            refuse(NetworkAction::Push, &ready, &tracking(Some("main"), None)),
            Some(NetworkRefusal::TrackingRequired)
        );
        assert_eq!(
            refuse(
                NetworkAction::Pull,
                &ready,
                &tracking(Some("main"), Some("other/main"))
            ),
            Some(NetworkRefusal::RemoteMissing)
        );
        assert_eq!(
            refuse(
                NetworkAction::Fetch,
                &binding(&[]),
                &tracking(Some("main"), None)
            ),
            Some(NetworkRefusal::NoRemote)
        );
        let stale = binding(&[("origin", TransportMode::System, false)]);
        assert_eq!(
            refuse(
                NetworkAction::Pull,
                &stale,
                &tracking(Some("main"), Some("origin/main"))
            ),
            Some(NetworkRefusal::NeedsAttention)
        );
        let anonymous = binding(&[("origin", TransportMode::Anonymous, true)]);
        assert_eq!(
            refuse(
                NetworkAction::Push,
                &anonymous,
                &tracking(Some("main"), Some("origin/main"))
            ),
            Some(NetworkRefusal::AnonymousReadOnly)
        );
    }

    fn operation(state: OperationState, steps: &[&str]) -> NetworkOperation {
        NetworkOperation {
            id: "git_1".to_owned(),
            runtime_id: "server_1".to_owned(),
            action: NetworkAction::Push,
            state,
            completed_steps: steps.iter().map(|step| (*step).to_owned()).collect(),
            error: None,
        }
    }

    #[test]
    fn snapshots_only_move_forward() {
        let planned = operation(OperationState::Planned, &[]);
        let running = accept_snapshot(&planned, operation(OperationState::Running, &["validated"]))
            .expect("progress");
        assert_eq!(
            accept_snapshot(&running, operation(OperationState::Running, &[])),
            Err(SnapshotRejection::InvalidTransition),
            "a completed step never disappears"
        );
        let unknown = accept_snapshot(
            &running,
            operation(OperationState::OutcomeUnknown, &["validated"]),
        )
        .expect("a terminal state");
        assert_eq!(
            accept_snapshot(
                &unknown,
                operation(OperationState::Succeeded, &["validated"])
            ),
            Err(SnapshotRejection::InvalidTransition)
        );
        let mut other = operation(OperationState::Running, &[]);
        other.runtime_id = "server_2".to_owned();
        assert_eq!(
            accept_snapshot(&planned, other),
            Err(SnapshotRejection::Mismatch)
        );
        assert!(OperationState::parse("outcome-unknown").is_some_and(|state| !state.is_open()));
        assert_eq!(OperationState::parse("later"), None);
    }

    #[test]
    fn a_merge_with_a_head_or_a_rebase_with_a_ref_is_in_progress() {
        let file = |path: &str, index: char, working: char| FileStatus {
            path: path.to_owned(),
            index,
            working,
        };
        let mut changes = GitChanges {
            files: vec![
                file("b", 'U', 'U'),
                file("a", 'A', 'A'),
                file("c", 'M', ' '),
            ],
            ..GitChanges::default()
        };
        assert_eq!(changes.in_progress(), None);
        assert_eq!(changes.conflicted(), vec!["a".to_owned(), "b".to_owned()]);
        changes.merge = Some(MergeState::default());
        assert_eq!(changes.in_progress(), None, "a merge without a head");
        changes.rebase = Some(RebaseState {
            head_name: None,
            onto: Some("abc".to_owned()),
        });
        assert_eq!(changes.in_progress(), Some(Integration::Rebase));
        changes.merge = Some(MergeState {
            head: "def".to_owned(),
            message: None,
        });
        assert_eq!(changes.in_progress(), Some(Integration::Merge));
    }
}
