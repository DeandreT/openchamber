//! Worktree creation and removal, and moving a session to another
//! directory, through the server's own operations.
//!
//! The routes are the web client's: `POST` and `DELETE /api/git/worktrees`,
//! `GET /api/git/worktrees/bootstrap-status` and `PUT
//! /api/git/worktrees/title` (`packages/ui/src/lib/gitApiHttp.ts`), and
//! OpenCode's `session.move` (`POST /api/session/:id/move`). OpenCode 2.x
//! relocates the session only; files stay where they are. No call retries.

use openchamber_native_core::ids::{ProjectId, SessionId};
use openchamber_native_core::session_moves::WorktreeSetup;
use reqwest::{Method, StatusCode, Url};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, read_json};
use crate::error::TransportError;

/// A new worktree to create from a project's repository (`toCreatePayload`
/// for a quick worktree).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewWorktree {
    /// The folder and branch name, such as `agile-chipmunk`.
    pub name: String,
    /// The branch or commit to start from; the server's default when absent.
    pub start_ref: Option<String>,
    /// Setup commands joined with `&&`, run after the checkout.
    pub start_command: Option<String>,
    /// The generator made `name`, so the agent may rename the branch.
    pub generated: bool,
}

/// What the server created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedWorktree {
    pub name: String,
    pub branch: String,
    pub path: String,
}

/// Where a worktree's background setup stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bootstrap {
    Pending { git_ready: bool },
    Ready,
    Failed(Option<String>),
}

#[derive(Deserialize)]
struct CreateBody {
    name: Option<String>,
    branch: Option<String>,
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapBody {
    status: String,
    phase: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct RemoveBody {
    success: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetupBody {
    trust: SetupTrust,
    setup_worktree: Vec<String>,
    shared: SharedSetup,
    personal: PersonalSetup,
}

#[derive(Deserialize)]
struct SetupTrust {
    hash: Option<String>,
    trusted: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SharedSetup {
    setup_worktree: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersonalSetup {
    setup_worktree: Vec<String>,
    setup_worktree_mode: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
}

impl ServerClient {
    fn worktree_url(&self, route: &str, directory: &str) -> Url {
        let mut url = self.endpoint().url(route);
        url.query_pairs_mut().append_pair("directory", directory);
        url
    }

    /// Creates a worktree of the repository at `project_directory`. The
    /// answer comes once its folder exists; Git setup finishes in the
    /// background (`returnAfterDirectoryCreated`).
    pub async fn create_worktree(
        &self,
        project_directory: &str,
        worktree: &NewWorktree,
    ) -> Result<CreatedWorktree, TransportError> {
        let mut body = Map::new();
        body.insert("mode".to_owned(), json!("new"));
        body.insert("worktreeName".to_owned(), json!(worktree.name));
        body.insert("branchName".to_owned(), json!(worktree.name));
        if let Some(start_ref) = worktree
            .start_ref
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            body.insert("startRef".to_owned(), json!(start_ref));
        }
        if let Some(command) = worktree
            .start_command
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            body.insert("startCommand".to_owned(), json!(command));
        }
        body.insert("returnAfterDirectoryCreated".to_owned(), json!(true));
        if worktree.generated {
            body.insert("generatedBranchName".to_owned(), json!(worktree.name));
        }
        let response = self
            .authorized(
                Method::POST,
                self.worktree_url("api/git/worktrees", project_directory),
            )
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let created: CreateBody = read_json(response).await?;
        let (Some(name), Some(path)) = (
            created.name.filter(|name| !name.is_empty()),
            created.path.filter(|path| !path.is_empty()),
        ) else {
            return Err(TransportError::Malformed(
                "the created worktree has no name or path".to_owned(),
            ));
        };
        Ok(CreatedWorktree {
            name,
            branch: created.branch.unwrap_or_default(),
            path,
        })
    }

    /// The worktree setup commands of the project at `project_path`.
    /// `getProjectSetup` reads a failed answer as no setup, and so does this.
    pub async fn worktree_setup(&self, project_path: &str) -> WorktreeSetup {
        let Some(project) = ProjectId::from_path(project_path) else {
            return WorktreeSetup::default();
        };
        let Ok(body) = self
            .get_json::<SetupBody>(self.project_setup_url(&project), None)
            .await
        else {
            return WorktreeSetup::default();
        };
        WorktreeSetup {
            commands: body.setup_worktree,
            shared_commands: body.shared.setup_worktree,
            personal_commands: body.personal.setup_worktree,
            personal_replaces: body.personal.setup_worktree_mode == "replace",
            // A null hash has nothing to trust.
            shared_trusted: body.trust.trusted || body.trust.hash.is_none(),
        }
    }

    /// Where the setup of the worktree at `directory` stands.
    pub async fn worktree_bootstrap(&self, directory: &str) -> Result<Bootstrap, TransportError> {
        let body: BootstrapBody = self
            .get_json(
                self.worktree_url("api/git/worktrees/bootstrap-status", directory),
                None,
            )
            .await?;
        Ok(match body.status.as_str() {
            "ready" => Bootstrap::Ready,
            "failed" => Bootstrap::Failed(body.error.filter(|error| !error.is_empty())),
            "pending" => Bootstrap::Pending {
                git_ready: matches!(body.phase.as_deref(), Some("git-ready" | "setup-ready")),
            },
            _ => {
                return Err(TransportError::Malformed(
                    "unknown worktree bootstrap status".to_owned(),
                ));
            }
        })
    }

    /// Removes the worktree at `worktree` from the repository at
    /// `project_directory`, with its local branch when asked.
    pub async fn remove_worktree(
        &self,
        project_directory: &str,
        worktree: &str,
        delete_local_branch: bool,
    ) -> Result<(), TransportError> {
        let response = self
            .authorized(
                Method::DELETE,
                self.worktree_url("api/git/worktrees", project_directory),
            )
            .json(&json!({ "directory": worktree, "deleteLocalBranch": delete_local_branch }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let body: RemoveBody = read_json(response).await?;
        if body.success != Some(true) {
            return Err(TransportError::Malformed(
                "the server did not confirm the worktree removal".to_owned(),
            ));
        }
        Ok(())
    }

    /// Names the task of the worktree at `directory`. `by_user` keeps the
    /// name over the agent's `work.name`.
    pub async fn set_worktree_title(
        &self,
        directory: &str,
        title: &str,
        by_user: bool,
    ) -> Result<(), TransportError> {
        let response = self
            .authorized(
                Method::PUT,
                self.worktree_url("api/git/worktrees/title", directory),
            )
            .json(&json!({ "title": title, "by": if by_user { "user" } else { "agent" } }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let status = response.status();
        if !status.is_success() {
            // The server's explanation is a short sentence meant for people.
            let message = response
                .json::<ErrorBody>()
                .await
                .ok()
                .and_then(|body| body.error);
            return Err(match message {
                Some(message) => TransportError::Malformed(message),
                None => TransportError::Http {
                    status: status.as_u16(),
                },
            });
        }
        Ok(())
    }

    /// Moves one session to `directory` (`session.move`). OpenCode admits
    /// the move durably before it answers, so a lost answer leaves the
    /// outcome to the next read or `session.moved` event.
    pub async fn move_session(
        &self,
        id: &SessionId,
        directory: &str,
    ) -> Result<(), TransportError> {
        let mut url = self.endpoint().url("api/session/");
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .pop_if_empty()
            .extend([id.as_str(), "move"]);
        let status = self
            .authorized(Method::POST, url)
            .json(&json!({ "directory": open_code_directory(directory) }))
            .send()
            .await
            .map_err(TransportError::from_request)?
            .status();
        if status.is_redirection() {
            return Err(TransportError::Redirected);
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(TransportError::AuthenticationRequired);
        }
        if status != StatusCode::NO_CONTENT && !status.is_success() {
            return Err(TransportError::Http {
                status: status.as_u16(),
            });
        }
        Ok(())
    }
}

/// `toOpenCodeDirectory`: a UNC path (`//host/share`) goes to OpenCode
/// with backslashes.
fn open_code_directory(directory: &str) -> String {
    let unc =
        directory.starts_with("//") && directory.as_bytes().get(2).is_some_and(|b| *b != b'/');
    if unc {
        directory.replace('/', "\\")
    } else {
        directory.to_owned()
    }
}

/// Whether a failure leaves the request's outcome unknown: the server may
/// have acted before the answer was lost (`isAmbiguousSendFailure`).
pub fn outcome_unknown(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Unreachable(_)
            | TransportError::TimedOut
            | TransportError::Stream(_)
            | TransportError::Http {
                status: 408 | 503 | 504
            }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_paths_reach_opencode_with_backslashes() {
        assert_eq!(open_code_directory("//host/share/a"), "\\\\host\\share\\a");
        assert_eq!(open_code_directory("/home/a"), "/home/a");
        assert_eq!(open_code_directory("///a"), "///a");
    }

    #[test]
    fn only_lost_answers_leave_the_outcome_unknown() {
        assert!(outcome_unknown(&TransportError::TimedOut));
        assert!(outcome_unknown(&TransportError::Http { status: 503 }));
        assert!(!outcome_unknown(&TransportError::Http { status: 400 }));
        assert!(!outcome_unknown(&TransportError::AuthenticationRequired));
    }
}
