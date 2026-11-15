//! The Git panel's routes, as `lib/gitApiHttp.ts` and
//! `lib/boundGitNetworkOperation.ts` call them: the full status, path
//! diffs, staging, discarding, commits, branches, the repository's transport
//! binding and planned network operations.
//!
//! Every route takes the repository in `?directory=`. Push, pull and fetch go
//! through planned operations only: plan (`201`), execute, read and cancel by
//! the operation's ID. The legacy push, pull and fetch routes always answer
//! 409, so they are never called. No call here retries; a lost execute
//! answer is resolved by reading the same operation ID again.

use std::collections::HashMap;

use crate::request::RequestBuilder;
use openchamber_native_core::git_changes::{
    ChangesRead, FileStatus, GitChanges, Integration, LineStat, MergeState, NetworkAction,
    NetworkOperation, NetworkRequest, OperationError, OperationState, RebaseState, RemoteEndpoint,
    RemoteGrant, RepositoryBinding, TransportMode,
};
use openchamber_native_core::git_identity::{Identity, IdentityTransport};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::files::{Refusal, parse, read_answer};

/// The most paths one batch diff request carries (`GIT_DIFFS_MAX_PATHS`).
pub const DIFFS_MAX_PATHS: usize = 1000;

/// Why a Git request did not succeed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GitError {
    /// The server declined, with its stable code and message when it sent
    /// them.
    #[error("the server refused the Git request (HTTP {status})")]
    Refused {
        status: u16,
        code: Option<String>,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl GitError {
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Refused { message, .. } => message.as_deref(),
            Self::Transport(_) => None,
        }
    }
}

impl From<Refusal> for GitError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused {
            status: refusal.status.as_u16(),
            code: refusal.reason,
            message: refusal.message,
        }
    }
}

/// A path's patch, or why the server shows none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathDiff {
    Patch(String),
    /// A nested repository: its own changes are read in that repository.
    NestedRepository,
    /// A folder of untracked files, too many to list.
    UntrackedDirectory,
    /// The path is no longer changed.
    NotFound,
}

/// What a commit created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitCreated {
    pub commit: String,
    pub branch: String,
    /// The commit had no changes to record.
    pub empty: bool,
}

/// How a merge, rebase or their continuation ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntegrationOutcome {
    /// The operation stopped on conflicts in these paths.
    pub conflict: bool,
    pub conflict_files: Vec<String>,
}

/// What the conflict dialog shows and hands to a session
/// (`GET /api/git/conflict-details`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConflictDetails {
    pub operation: Option<Integration>,
    pub head_info: String,
    pub status_porcelain: String,
    pub unmerged_files: Vec<String>,
    pub diff: String,
}

/// The repository's branches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Branches {
    pub current: Option<String>,
    /// Local branch names in the server's order.
    pub local: Vec<String>,
    /// Remote-tracking branches without their `remotes/` prefix.
    pub remote: Vec<String>,
    /// Each remote's default branch, read from its `HEAD` symbolic ref.
    pub default_branches: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusBody {
    is_git_repository: Option<bool>,
    current: Option<String>,
    tracking: Option<String>,
    #[serde(default)]
    ahead: u64,
    #[serde(default)]
    behind: u64,
    #[serde(default)]
    unborn: bool,
    #[serde(default)]
    files: Vec<FileBody>,
    diff_stats: Option<DiffStatsBody>,
    merge_in_progress: Option<MergeBody>,
    rebase_in_progress: Option<RebaseBody>,
}

#[derive(Deserialize)]
struct MergeBody {
    #[serde(default)]
    head: String,
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RebaseBody {
    head_name: Option<String>,
    onto: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IntegrationBody {
    #[serde(default)]
    conflict: bool,
    #[serde(default)]
    conflict_files: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConflictBody {
    operation: Option<String>,
    #[serde(default)]
    head_info: String,
    #[serde(default)]
    status_porcelain: String,
    #[serde(default)]
    unmerged_files: Vec<String>,
    #[serde(default)]
    diff: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdentityBody {
    id: String,
    name: String,
    user_name: String,
    user_email: String,
    account: Option<Value>,
    transport: Option<String>,
    ssh_credential_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthorBody {
    user_name: Option<String>,
    user_email: Option<String>,
}

#[derive(Deserialize)]
struct FileBody {
    path: String,
    index: String,
    working_dir: String,
}

#[derive(Default, Deserialize)]
struct DiffStatsBody {
    #[serde(default)]
    staged: HashMap<String, StatBody>,
    #[serde(default)]
    working: HashMap<String, StatBody>,
}

#[derive(Deserialize)]
struct StatBody {
    #[serde(default)]
    insertions: u64,
    #[serde(default)]
    deletions: u64,
}

#[derive(Deserialize)]
struct DiffBody {
    diff: String,
}

#[derive(Deserialize)]
struct DiffsBody {
    diffs: Vec<DiffEntryBody>,
}

#[derive(Deserialize)]
struct DiffEntryBody {
    path: String,
    diff: Option<String>,
    code: Option<String>,
}

#[derive(Deserialize)]
struct SuccessBody {
    success: bool,
}

#[derive(Deserialize)]
struct CommitBody {
    success: bool,
    #[serde(default)]
    commit: String,
    #[serde(default)]
    branch: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchesBody {
    all: Vec<String>,
    current: Option<String>,
    #[serde(default)]
    default_branches: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BindingBody {
    repository: RepositoryBody,
    revision: u64,
    binding: Option<BoundBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryBody {
    repository_id: String,
    config_revision: String,
    #[serde(default)]
    remotes: Vec<RemoteBody>,
}

#[derive(Deserialize)]
struct RemoteBody {
    name: String,
    fetch: EndpointBody,
    push: EndpointBody,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EndpointBody {
    display_url: String,
    fingerprint: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BoundBody {
    repository_id: String,
    revision: u64,
    #[serde(default)]
    remotes: Vec<GrantBody>,
}

#[derive(Deserialize)]
struct GrantBody {
    name: String,
    mode: String,
    readiness: String,
    fetch: FingerprintBody,
    push: FingerprintBody,
}

#[derive(Deserialize)]
struct FingerprintBody {
    fingerprint: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OperationBody {
    operation_id: String,
    runtime_identity: RuntimeBody,
    target: TargetBody,
    #[serde(default)]
    completed_steps: Vec<String>,
    state: String,
    error: Option<ErrorBody>,
}

#[derive(Deserialize)]
struct RuntimeBody {
    id: String,
}

#[derive(Deserialize)]
struct TargetBody {
    operation: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: Option<String>,
    message: Option<String>,
}

impl ServerClient {
    fn git_url(&self, route: &str, directory: Option<&str>) -> Url {
        let mut url = self.endpoint().url(route);
        if let Some(directory) = directory {
            url.query_pairs_mut().append_pair("directory", directory);
        }
        url
    }

    fn git_request(&self, method: Method, route: &str, directory: &str) -> RequestBuilder {
        self.authorized(method, self.git_url(route, Some(directory)))
    }

    /// The repository's full status with per-path line counts, or that the
    /// directory is no repository.
    pub async fn git_changes(&self, directory: &str) -> Result<ChangesRead, GitError> {
        let body: StatusBody =
            git_json(self.git_request(Method::GET, "api/git/status", directory)).await?;
        Ok(changes_read(body))
    }

    /// The unified patch of `path`, staged or in the worktree, with three
    /// lines of context.
    pub async fn git_diff(
        &self,
        directory: &str,
        path: &str,
        staged: bool,
    ) -> Result<PathDiff, GitError> {
        let mut url = self.git_url("api/git/diff", Some(directory));
        url.query_pairs_mut()
            .append_pair("path", path)
            .append_pair("context", "3");
        if staged {
            url.query_pairs_mut().append_pair("staged", "true");
        }
        let request = self.authorized(Method::GET, url);
        let response = request.send().await.map_err(TransportError::from_request)?;
        match read_answer(response).await? {
            Ok(bytes) => Ok(PathDiff::Patch(parse::<DiffBody>(&bytes)?.diff)),
            Err(refusal) => match (refusal.status.as_u16(), refusal.reason.as_deref()) {
                (422, Some("nested_repository")) => Ok(PathDiff::NestedRepository),
                (422, Some("untracked_directory")) => Ok(PathDiff::UntrackedDirectory),
                (404, Some("path_not_found")) => Ok(PathDiff::NotFound),
                _ => Err(refusal.into()),
            },
        }
    }

    /// The patches of many paths of one side in one request, at most
    /// [`DIFFS_MAX_PATHS`]; a path the server could not diff carries its
    /// own outcome and never fails the others.
    pub async fn git_diffs(
        &self,
        directory: &str,
        paths: &[String],
        staged: bool,
    ) -> Result<Vec<(String, PathDiff)>, GitError> {
        let request = self
            .git_request(Method::POST, "api/git/diffs", directory)
            .json(&json!({ "paths": paths, "staged": staged, "context": 3 }));
        let body: DiffsBody = git_json(request).await?;
        Ok(body
            .diffs
            .into_iter()
            .map(|entry| {
                let diff = match (entry.diff, entry.code.as_deref()) {
                    (Some(diff), _) => PathDiff::Patch(diff),
                    (None, Some("nested_repository")) => PathDiff::NestedRepository,
                    (None, Some("untracked_directory")) => PathDiff::UntrackedDirectory,
                    (None, _) => PathDiff::NotFound,
                };
                (entry.path, diff)
            })
            .collect())
    }

    /// Stages `paths`.
    pub async fn stage(&self, directory: &str, paths: &[String]) -> Result<(), GitError> {
        let request = self
            .git_request(Method::POST, "api/git/stage", directory)
            .json(&json!({ "paths": paths }));
        expect_success(request).await
    }

    /// Unstages `paths`, keeping their worktree changes.
    pub async fn unstage(&self, directory: &str, paths: &[String]) -> Result<(), GitError> {
        let request = self
            .git_request(Method::POST, "api/git/unstage", directory)
            .json(&json!({ "paths": paths }));
        expect_success(request).await
    }

    /// Discards the worktree changes of `path` and keeps its staged part
    /// (`revertGitFile` with `scope: 'working'`, as a change row sends it);
    /// without a scope the server also unstages.
    pub async fn discard(&self, directory: &str, path: &str) -> Result<(), GitError> {
        let request = self
            .git_request(Method::POST, "api/git/revert", directory)
            .json(&json!({ "path": path, "scope": "working" }));
        expect_success(request).await
    }

    /// Discards every change of `path`, staged and unstaged (`revertGitFile`
    /// with `scope: 'all'`, as Revert all sends it for each listed path).
    pub async fn discard_all_changes(&self, directory: &str, path: &str) -> Result<(), GitError> {
        let request = self
            .git_request(Method::POST, "api/git/revert", directory)
            .json(&json!({ "path": path, "scope": "all" }));
        expect_success(request).await
    }

    /// Commits what is staged with `message`.
    pub async fn commit(&self, directory: &str, message: &str) -> Result<CommitCreated, GitError> {
        let request = self
            .git_request(Method::POST, "api/git/commit", directory)
            .json(&json!({ "message": message, "addAll": false }));
        let body: CommitBody = git_json(request).await?;
        if !body.success {
            return Err(
                TransportError::Malformed("the commit was not confirmed".to_owned()).into(),
            );
        }
        Ok(CommitCreated {
            empty: body.commit.is_empty(),
            commit: body.commit,
            branch: body.branch,
        })
    }

    pub async fn branches(&self, directory: &str) -> Result<Branches, GitError> {
        let body: BranchesBody =
            git_json(self.git_request(Method::GET, "api/git/branches", directory)).await?;
        let mut branches = Branches {
            current: body.current.filter(|current| !current.is_empty()),
            default_branches: body.default_branches,
            ..Branches::default()
        };
        for name in body.all {
            match name.strip_prefix("remotes/") {
                Some(remote) => branches.remote.push(remote.to_owned()),
                None => branches.local.push(name),
            }
        }
        Ok(branches)
    }

    pub async fn create_branch(&self, directory: &str, name: &str) -> Result<(), GitError> {
        let request = self
            .git_request(Method::POST, "api/git/branches", directory)
            .json(&json!({ "name": name }));
        expect_success(request).await
    }

    pub async fn checkout_branch(&self, directory: &str, name: &str) -> Result<(), GitError> {
        let request = self
            .git_request(Method::POST, "api/git/checkout", directory)
            .json(&json!({ "branch": name }));
        expect_success(request).await
    }

    pub async fn rename_branch(
        &self,
        directory: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), GitError> {
        let request = self
            .git_request(Method::PUT, "api/git/branches/rename", directory)
            .json(&json!({ "oldName": old_name, "newName": new_name }));
        expect_success(request).await
    }

    pub async fn delete_branch(&self, directory: &str, name: &str) -> Result<(), GitError> {
        let request = self
            .git_request(Method::DELETE, "api/git/branches", directory)
            .json(&json!({ "branch": name }));
        expect_success(request).await
    }

    /// The names of the repository's remotes (`GET /api/git/remotes`).
    pub async fn remote_names(&self, directory: &str) -> Result<Vec<String>, GitError> {
        #[derive(Deserialize)]
        struct Remote {
            name: String,
        }
        let remotes: Vec<Remote> =
            git_json(self.git_request(Method::GET, "api/git/remotes", directory)).await?;
        Ok(remotes.into_iter().map(|remote| remote.name).collect())
    }

    /// The commit `reference` points at (`GET /api/git/log` limited to one
    /// entry), or `None` when the log is empty.
    pub async fn latest_commit(
        &self,
        directory: &str,
        reference: &str,
    ) -> Result<Option<String>, GitError> {
        #[derive(Deserialize)]
        struct Log {
            latest: Option<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            hash: String,
        }
        let mut url = self.git_url("api/git/log", Some(directory));
        url.query_pairs_mut()
            .append_pair("to", reference)
            .append_pair("maxCount", "1");
        let log: Log = git_json(self.authorized(Method::GET, url)).await?;
        Ok(log.latest.map(|entry| entry.hash.trim().to_owned()))
    }

    /// The repositories under `root` (`GET /api/fs/git-dirs`). A server
    /// without the route answers 501, which lists none.
    pub async fn git_directories(&self, root: &str) -> Result<Vec<String>, GitError> {
        #[derive(Deserialize)]
        struct Listed {
            repositories: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            path: Option<String>,
        }
        let mut url = self.git_url("api/fs/git-dirs", Some(root));
        url.query_pairs_mut().append_pair("path", root);
        let response = self
            .authorized(Method::GET, url)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let listed: Listed = match read_answer(response).await? {
            Ok(bytes) => parse(&bytes)?,
            Err(refusal) if refusal.status.as_u16() == 501 => return Ok(Vec::new()),
            Err(refusal) => return Err(refusal.into()),
        };
        Ok(listed
            .repositories
            .into_iter()
            .filter_map(|entry| entry.path)
            .filter_map(|path| openchamber_native_core::path::normalize(&path))
            .collect())
    }

    /// The stored commit identities (`GET /api/git/identities`).
    pub async fn git_identities(&self) -> Result<Vec<Identity>, GitError> {
        let request = self.authorized(Method::GET, self.git_url("api/git/identities", None));
        let bodies: Vec<IdentityBody> = git_json(request).await?;
        Ok(bodies.into_iter().map(identity).collect())
    }

    /// The machine's own author, as the System identity offers it.
    pub async fn system_identity(&self) -> Result<Identity, GitError> {
        let request = self.authorized(Method::GET, self.git_url("api/git/global-identity", None));
        let author: Option<AuthorBody> = git_json(request).await?;
        let author = author.unwrap_or(AuthorBody {
            user_name: None,
            user_email: None,
        });
        Ok(Identity::system(author.user_name, author.user_email))
    }

    /// The author the repository commits as: its own, else the machine's.
    pub async fn current_author(
        &self,
        directory: &str,
    ) -> Result<(Option<String>, Option<String>), GitError> {
        let author: Option<AuthorBody> =
            git_json(self.git_request(Method::GET, "api/git/current-identity", directory)).await?;
        Ok(author.map_or((None, None), |author| (author.user_name, author.user_email)))
    }

    /// Writes the identity's author into the repository; the System
    /// identity removes the repository's own author.
    pub async fn set_identity(&self, directory: &str, profile_id: &str) -> Result<(), GitError> {
        let request = self
            .git_request(Method::POST, "api/git/set-identity", directory)
            .json(&json!({ "profileId": profile_id }));
        expect_success(request).await
    }

    /// Merges `branch` into the current branch, or rebases the current
    /// branch onto `branch`. A conflict is an outcome, never an error.
    pub async fn integrate(
        &self,
        directory: &str,
        integration: Integration,
        branch: &str,
    ) -> Result<IntegrationOutcome, GitError> {
        let request = match integration {
            Integration::Merge => self
                .git_request(Method::POST, "api/git/merge", directory)
                .json(&json!({ "branch": branch })),
            Integration::Rebase => self
                .git_request(Method::POST, "api/git/rebase", directory)
                .json(&json!({ "onto": branch })),
        };
        let body: IntegrationBody = git_json(request).await?;
        Ok(outcome(body))
    }

    /// Continues the merge or rebase in progress once its conflicts are
    /// resolved; a further conflict is its outcome.
    pub async fn continue_integration(
        &self,
        directory: &str,
        integration: Integration,
    ) -> Result<IntegrationOutcome, GitError> {
        let route = match integration {
            Integration::Merge => "api/git/merge/continue",
            Integration::Rebase => "api/git/rebase/continue",
        };
        let body: IntegrationBody =
            git_json(self.git_request(Method::POST, route, directory)).await?;
        Ok(outcome(body))
    }

    /// Abandons the merge or rebase in progress.
    pub async fn abort_integration(
        &self,
        directory: &str,
        integration: Integration,
    ) -> Result<(), GitError> {
        let route = match integration {
            Integration::Merge => "api/git/merge/abort",
            Integration::Rebase => "api/git/rebase/abort",
        };
        let _: Value = git_json(self.git_request(Method::POST, route, directory)).await?;
        Ok(())
    }

    /// The conflicted paths, the in-progress head and the current diff.
    pub async fn conflict_details(&self, directory: &str) -> Result<ConflictDetails, GitError> {
        let body: ConflictBody =
            git_json(self.git_request(Method::GET, "api/git/conflict-details", directory)).await?;
        Ok(ConflictDetails {
            operation: match body.operation.as_deref() {
                Some("merge") => Some(Integration::Merge),
                Some("rebase") => Some(Integration::Rebase),
                _ => None,
            },
            head_info: body.head_info,
            status_porcelain: body.status_porcelain,
            unmerged_files: body.unmerged_files,
            diff: body.diff,
        })
    }

    /// The repository's transport binding; an unconfigured repository holds
    /// a ready System grant for each remote, as `effectiveRepositoryBinding`
    /// derives it.
    pub async fn repository_binding(&self, directory: &str) -> Result<RepositoryBinding, GitError> {
        let body: BindingBody =
            git_json(self.git_request(Method::GET, "api/source-control/binding", directory))
                .await?;
        binding(body).map_err(GitError::from)
    }

    /// Plans an operation; the server stores its immutable plan.
    pub async fn plan_operation(
        &self,
        request: &NetworkRequest,
    ) -> Result<NetworkOperation, GitError> {
        let request = self
            .authorized(
                Method::POST,
                self.git_url("api/git/network-operations", None),
            )
            .json(&plan_body(request));
        operation(git_json(request).await?)
    }

    /// Starts or joins the planned operation's execution; answers once it
    /// finished.
    pub async fn execute_operation(&self, id: &str) -> Result<NetworkOperation, GitError> {
        let request = self.authorized(Method::POST, self.operation_url(id, Some("execute")));
        operation(git_json(request).await?)
    }

    /// The operation's current snapshot.
    pub async fn read_operation(&self, id: &str) -> Result<NetworkOperation, GitError> {
        let request = self.authorized(Method::GET, self.operation_url(id, None));
        operation(git_json(request).await?)
    }

    /// Asks the server to cancel; idempotent.
    pub async fn cancel_operation(&self, id: &str) -> Result<NetworkOperation, GitError> {
        let request = self.authorized(Method::POST, self.operation_url(id, Some("cancel")));
        operation(git_json(request).await?)
    }

    fn operation_url(&self, id: &str, action: Option<&str>) -> Url {
        let mut url = self.endpoint().url("api/git/network-operations/");
        {
            let mut segments = url
                .path_segments_mut()
                .expect("HTTP URLs have path segments");
            segments.pop_if_empty().push(id);
            if let Some(action) = action {
                segments.push(action);
            }
        }
        url
    }
}

async fn git_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, GitError> {
    let response = request.send().await.map_err(TransportError::from_request)?;
    match read_answer(response).await? {
        Ok(bytes) => Ok(parse(&bytes)?),
        Err(refusal) => Err(refusal.into()),
    }
}

async fn expect_success(request: RequestBuilder) -> Result<(), GitError> {
    let body: SuccessBody = git_json(request).await?;
    if body.success {
        Ok(())
    } else {
        Err(TransportError::Malformed("the server did not confirm the change".to_owned()).into())
    }
}

fn identity(body: IdentityBody) -> Identity {
    Identity {
        id: body.id,
        name: body.name,
        user_name: body.user_name,
        user_email: body.user_email,
        transport: match body.transport.as_deref() {
            Some("account") => IdentityTransport::Account,
            Some("ssh") => IdentityTransport::Ssh,
            Some("anonymous") => IdentityTransport::Anonymous,
            _ => IdentityTransport::System,
        },
        has_account: body
            .account
            .as_ref()
            .is_some_and(|account| !account.is_null()),
        has_ssh_key: body
            .ssh_credential_id
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty()),
    }
}

fn outcome(body: IntegrationBody) -> IntegrationOutcome {
    IntegrationOutcome {
        conflict: body.conflict,
        conflict_files: body.conflict_files,
    }
}

fn letter(value: &str) -> char {
    value.chars().next().unwrap_or(' ')
}

fn changes_read(body: StatusBody) -> ChangesRead {
    if body.is_git_repository == Some(false) {
        return ChangesRead::NotRepository;
    }
    let stats = |entries: HashMap<String, StatBody>| {
        entries
            .into_iter()
            .map(|(path, stat)| {
                (
                    path,
                    LineStat {
                        insertions: stat.insertions,
                        deletions: stat.deletions,
                    },
                )
            })
            .collect()
    };
    let diff_stats = body.diff_stats.unwrap_or_default();
    ChangesRead::Repository(Box::new(GitChanges {
        branch: body
            .current
            .map(|branch| branch.trim().to_owned())
            .filter(|branch| !branch.is_empty()),
        tracking: body.tracking.filter(|tracking| !tracking.is_empty()),
        ahead: body.ahead,
        behind: body.behind,
        unborn: body.unborn,
        files: body
            .files
            .into_iter()
            .map(|file| FileStatus {
                path: file.path,
                index: letter(&file.index),
                working: letter(&file.working_dir),
            })
            .collect(),
        staged_stats: stats(diff_stats.staged),
        working_stats: stats(diff_stats.working),
        merge: body.merge_in_progress.map(|merge| MergeState {
            head: merge.head,
            message: merge.message.filter(|message| !message.is_empty()),
        }),
        rebase: body.rebase_in_progress.map(|rebase| RebaseState {
            head_name: rebase.head_name.filter(|name| !name.is_empty()),
            onto: rebase.onto.filter(|onto| !onto.is_empty()),
        }),
    }))
}

fn endpoint(body: EndpointBody) -> RemoteEndpoint {
    RemoteEndpoint {
        display_url: body.display_url,
        fingerprint: body.fingerprint,
    }
}

fn binding(body: BindingBody) -> Result<RepositoryBinding, TransportError> {
    let repository = body.repository;
    let Some(bound) = body.binding else {
        return Ok(RepositoryBinding {
            repository_id: repository.repository_id,
            revision: body.revision,
            config_revision: repository.config_revision,
            remotes: repository
                .remotes
                .into_iter()
                .map(|remote| RemoteGrant {
                    name: remote.name,
                    fetch: endpoint(remote.fetch),
                    push: endpoint(remote.push),
                    mode: TransportMode::System,
                    ready: true,
                })
                .collect(),
        });
    };
    let mut remotes = Vec::new();
    for grant in bound.remotes {
        let mode = match grant.mode.as_str() {
            "system" => TransportMode::System,
            "managed" => TransportMode::Managed,
            "anonymous" => TransportMode::Anonymous,
            _ => {
                return Err(TransportError::Malformed(
                    "a remote grant has an unknown transport".to_owned(),
                ));
            }
        };
        // The endpoints the operation names are the repository's current
        // ones; a grant whose fingerprints moved needs attention.
        let Some(current) = repository
            .remotes
            .iter()
            .find(|remote| remote.name == grant.name)
        else {
            continue;
        };
        let ready = grant.readiness == "ready"
            && grant.fetch.fingerprint.as_deref() == Some(current.fetch.fingerprint.as_str())
            && grant.push.fingerprint.as_deref() == Some(current.push.fingerprint.as_str());
        remotes.push(RemoteGrant {
            name: grant.name,
            fetch: endpoint(current.fetch.clone()),
            push: endpoint(current.push.clone()),
            mode,
            ready,
        });
    }
    Ok(RepositoryBinding {
        repository_id: bound.repository_id,
        revision: bound.revision,
        config_revision: repository.config_revision,
        remotes,
    })
}

/// The plan body: exactly the fields the server's schema accepts for the
/// action, since it rejects unknown ones.
fn plan_body(request: &NetworkRequest) -> Value {
    let mut body = Map::new();
    let operation = match request.action {
        NetworkAction::Fetch => "fetch",
        NetworkAction::Pull => "pull",
        NetworkAction::Push => "push",
    };
    body.insert("operation".to_owned(), json!(operation));
    body.insert("directory".to_owned(), json!(request.directory));
    body.insert("repositoryId".to_owned(), json!(request.repository_id));
    body.insert(
        "bindingRevision".to_owned(),
        json!(request.binding_revision),
    );
    body.insert("configRevision".to_owned(), json!(request.config_revision));
    body.insert(
        "remote".to_owned(),
        json!({
            "name": request.remote,
            "endpoint": {
                "displayUrl": request.endpoint.display_url,
                "fingerprint": request.endpoint.fingerprint,
            },
        }),
    );
    match (&request.source_ref, &request.destination_ref) {
        (Some(source), Some(destination)) => {
            body.insert("sourceRef".to_owned(), json!(source));
            body.insert("destinationRef".to_owned(), json!(destination));
        }
        _ => {
            body.insert("fetchScope".to_owned(), json!("remote"));
        }
    }
    body.insert(
        "transportMode".to_owned(),
        json!(request.transport.as_str()),
    );
    Value::Object(body)
}

fn operation(body: OperationBody) -> Result<NetworkOperation, GitError> {
    let action = match body.target.operation.as_str() {
        "fetch" => NetworkAction::Fetch,
        "pull" => NetworkAction::Pull,
        "push" => NetworkAction::Push,
        _ => {
            return Err(TransportError::Malformed(
                "the operation is not a fetch, pull or push".to_owned(),
            )
            .into());
        }
    };
    let state = OperationState::parse(&body.state).ok_or_else(|| {
        TransportError::Malformed("the operation has an unknown state".to_owned())
    })?;
    Ok(NetworkOperation {
        id: body.operation_id,
        runtime_id: body.runtime_identity.id,
        action,
        state,
        completed_steps: body.completed_steps,
        error: body.error.map(|error| OperationError {
            code: error.code.unwrap_or_default(),
            message: error.message.unwrap_or_default(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_keeps_both_letters_and_line_counts() {
        let body: StatusBody = serde_json::from_str(
            r#"{"current":"main","tracking":"origin/main","unborn":false,"ahead":1,"behind":0,
               "files":[{"path":"a.txt","index":"M","working_dir":"M"},{"path":"n/","index":"?","working_dir":"?"}],
               "isClean":false,"diffStats":{"staged":{"a.txt":{"insertions":2,"deletions":1}},"working":{}},
               "mergeInProgress":null,"rebaseInProgress":{"headName":"x"}}"#,
        )
        .expect("test JSON is valid");
        let ChangesRead::Repository(changes) = changes_read(body) else {
            panic!("a repository");
        };
        assert_eq!(changes.tracking.as_deref(), Some("origin/main"));
        assert_eq!(
            (changes.files[0].index, changes.files[0].working),
            ('M', 'M')
        );
        assert_eq!(changes.staged_stats["a.txt"].insertions, 2);
        assert_eq!(changes.merge, None);
        assert_eq!(changes.in_progress(), Some(Integration::Rebase));
        assert_eq!(
            changes
                .rebase
                .as_ref()
                .and_then(|rebase| rebase.head_name.as_deref()),
            Some("x")
        );
        let none: StatusBody =
            serde_json::from_str(r#"{"isGitRepository":false}"#).expect("test JSON is valid");
        assert_eq!(changes_read(none), ChangesRead::NotRepository);
    }

    #[test]
    fn a_bound_grant_takes_the_current_endpoints_and_flags_moved_ones() {
        let body: BindingBody = serde_json::from_str(
            r#"{"repository":{"repositoryId":"repo_1","configRevision":"c","remotes":[
                 {"name":"origin","fetch":{"displayUrl":"https://h/r.git","fingerprint":"f"},"push":{"displayUrl":"https://h/r.git","fingerprint":"p"}},
                 {"name":"fork","fetch":{"displayUrl":"https://h/f.git","fingerprint":"ff"},"push":{"displayUrl":"https://h/f.git","fingerprint":"fp"}}]},
               "revision":4,
               "binding":{"repositoryId":"repo_1","revision":4,"remotes":[
                 {"name":"origin","mode":"managed","credentialId":"x","readiness":"ready","fetch":{"fingerprint":"f"},"push":{"fingerprint":"p"}},
                 {"name":"fork","mode":"system","readiness":"ready","fetch":{"fingerprint":"old"},"push":{"fingerprint":"fp"}}]}}"#,
        )
        .expect("test JSON is valid");
        let binding = binding(body).expect("valid");
        assert_eq!(binding.revision, 4);
        assert_eq!(binding.remotes[0].mode, TransportMode::Managed);
        assert!(binding.remotes[0].ready);
        assert!(!binding.remotes[1].ready, "a moved fetch endpoint");
        assert_eq!(binding.remotes[0].push.display_url, "https://h/r.git");
    }

    #[test]
    fn plan_bodies_carry_only_their_action_fields() {
        let request = NetworkRequest {
            action: NetworkAction::Fetch,
            directory: "/p".to_owned(),
            repository_id: "repo_1".to_owned(),
            binding_revision: 0,
            config_revision: "c".to_owned(),
            remote: "origin".to_owned(),
            endpoint: RemoteEndpoint {
                display_url: "http://127.0.0.1:1/r.git".to_owned(),
                fingerprint: "f".to_owned(),
            },
            source_ref: None,
            destination_ref: None,
            transport: TransportMode::System,
        };
        assert_eq!(
            plan_body(&request),
            json!({
                "operation": "fetch", "directory": "/p", "repositoryId": "repo_1",
                "bindingRevision": 0, "configRevision": "c",
                "remote": { "name": "origin", "endpoint": { "displayUrl": "http://127.0.0.1:1/r.git", "fingerprint": "f" } },
                "fetchScope": "remote", "transportMode": "system",
            })
        );
        let push = NetworkRequest {
            action: NetworkAction::Push,
            source_ref: Some("refs/heads/main".to_owned()),
            destination_ref: Some("refs/heads/main".to_owned()),
            ..request
        };
        let body = plan_body(&push);
        assert_eq!(body["operation"], "push");
        assert_eq!(body["sourceRef"], "refs/heads/main");
        assert!(body.get("fetchScope").is_none());
    }

    #[test]
    fn integration_and_conflict_answers_keep_their_files() {
        let body: IntegrationBody =
            serde_json::from_str(r#"{"success":false,"conflict":true,"conflictFiles":["a.txt"]}"#)
                .expect("test JSON is valid");
        assert_eq!(
            outcome(body),
            IntegrationOutcome {
                conflict: true,
                conflict_files: vec!["a.txt".to_owned()]
            }
        );
        let clean: IntegrationBody =
            serde_json::from_str(r#"{"success":true,"conflict":false}"#).expect("valid");
        assert!(!outcome(clean).conflict);
        let status: StatusBody = serde_json::from_str(
            r#"{"current":"main","files":[],"mergeInProgress":{"head":"abc1234","message":"Merge branch 'x'"},"rebaseInProgress":null}"#,
        )
        .expect("valid");
        let ChangesRead::Repository(changes) = changes_read(status) else {
            panic!("a repository");
        };
        assert_eq!(changes.in_progress(), Some(Integration::Merge));
        assert_eq!(
            changes.merge.and_then(|merge| merge.message),
            Some("Merge branch 'x'".to_owned())
        );
    }

    #[test]
    fn identities_keep_their_transport_and_account() {
        let bodies: Vec<IdentityBody> = serde_json::from_str(
            r#"[{"id":"w","name":"Work","userName":"Ada","userEmail":"a@w","transport":"account","account":{"provider":"github","instance":"github.com","accountId":"1"},"color":"info"},
               {"id":"k","name":"Key","userName":"Ada","userEmail":"a@k","transport":"ssh","sshCredentialId":"key_1"},
               {"id":"o","name":"Old","userName":"Ada","userEmail":"a@o"}]"#,
        )
        .expect("test JSON is valid");
        let identities: Vec<Identity> = bodies.into_iter().map(identity).collect();
        assert_eq!(identities[0].transport, IdentityTransport::Account);
        assert!(identities[0].has_account && identities[0].is_complete());
        assert!(identities[1].has_ssh_key);
        assert_eq!(
            identities[2].transport,
            IdentityTransport::System,
            "no transport is System"
        );
    }
}
