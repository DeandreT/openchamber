//! What the work status card reads about a session's project: the Git status
//! of its directory and the project's configured actions. Both are
//! OpenChamber routes, as `getGitStatus` in `lib/gitApiHttp.ts` and
//! `getProjectSetup` in `lib/openchamberConfig.ts` call them.

use std::collections::HashMap;

use openchamber_native_core::ids::ProjectId;
use openchamber_native_core::project_actions::{
    ActionSource, ProjectAction, ProjectSetup, SharedTrust,
};
use openchamber_native_core::repository::{
    Attention, Changes, GitStatusRead, LineCounts, RepositoryStatus,
};
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;

use crate::client::ServerClient;
use crate::error::TransportError;

/// `GET /api/git/status`: the fields the card reads.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitStatusBody {
    /// `false` only in the answer for a directory that is no repository.
    is_git_repository: Option<bool>,
    current: Option<String>,
    #[serde(default)]
    ahead: u64,
    #[serde(default)]
    behind: u64,
    #[serde(default)]
    files: Vec<Value>,
    diff_stats: Option<DiffStats>,
    attention_reason: Option<String>,
    rebase_in_progress: Option<Value>,
    merge_in_progress: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct DiffStats {
    #[serde(default)]
    staged: HashMap<String, LineStat>,
    #[serde(default)]
    working: HashMap<String, LineStat>,
}

#[derive(Debug, Default, Deserialize)]
struct LineStat {
    #[serde(default)]
    insertions: u64,
    #[serde(default)]
    deletions: u64,
}

/// `GET /api/projects/:projectId/config`: the merged actions and the trust
/// state of the shared ones.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectSetupBody {
    project_actions: Vec<ActionBody>,
    trust: Option<TrustBody>,
    shared: Option<SharedBody>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionBody {
    id: String,
    name: String,
    command: String,
    icon: Option<String>,
    run_in: Option<String>,
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TrustBody {
    hash: Option<String>,
    trusted: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SharedBody {
    path: String,
    #[serde(default)]
    setup_worktree: Vec<String>,
    #[serde(default)]
    project_actions: Vec<ActionBody>,
}

impl ServerClient {
    /// The URL of the Git status of `directory`.
    pub fn git_status_url(&self, directory: &str) -> Url {
        let mut url = self.endpoint().url("api/git/status");
        url.query_pairs_mut().append_pair("directory", directory);
        url
    }

    /// The Git status of `directory`, or that it is no repository.
    pub async fn git_status(&self, directory: &str) -> Result<GitStatusRead, TransportError> {
        let body: GitStatusBody = self.get_json(self.git_status_url(directory), None).await?;
        Ok(git_status_read(body))
    }

    /// The URL of a project's merged setup.
    pub fn project_setup_url(&self, project: &ProjectId) -> Url {
        self.endpoint()
            .url(&format!("api/projects/{}/config", project.as_str()))
    }

    /// The configured actions of the project at `project_path`, in order,
    /// and whether its shared commands still need trust.
    pub async fn project_setup(&self, project_path: &str) -> Result<ProjectSetup, TransportError> {
        let Some(project) = ProjectId::from_path(project_path) else {
            return Ok(ProjectSetup::default());
        };
        let body: ProjectSetupBody = self
            .get_json(self.project_setup_url(&project), None)
            .await?;
        project_setup(body)
    }

    /// A free loopback port for a static preview server (`/api/system/free-port`).
    pub async fn free_port(&self) -> Result<Option<u16>, TransportError> {
        let body: Value = self
            .get_json(self.endpoint().url("api/system/free-port"), None)
            .await?;
        Ok(body
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port > 0))
    }

    /// Records that this instance trusts the shared commands with `hash`
    /// (`updateProjectSetup` with `sharedTrustHash`).
    pub async fn trust_shared_setup(
        &self,
        project_path: &str,
        hash: &str,
    ) -> Result<(), TransportError> {
        let Some(project) = ProjectId::from_path(project_path) else {
            return Err(TransportError::Malformed("no project path".to_owned()));
        };
        let body =
            serde_json::json!({ "sharedTrustHash": hash, "projectPath": project_path.trim() });
        let response = self
            .authorized(reqwest::Method::PUT, self.project_setup_url(&project))
            .json(&body)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let _: Value = crate::client::read_json(response).await?;
        Ok(())
    }
}

fn git_status_read(body: GitStatusBody) -> GitStatusRead {
    if body.is_git_repository == Some(false) {
        return GitStatusRead::NotRepository;
    }
    let line_counts = body.diff_stats.map(|stats| {
        let mut counts = LineCounts::default();
        for stat in stats.staged.values().chain(stats.working.values()) {
            counts.additions += stat.insertions;
            counts.deletions += stat.deletions;
        }
        counts
    });
    // `attentionReason`, else a rebase or merge the status reports in progress.
    let attention = body
        .attention_reason
        .as_deref()
        .and_then(Attention::from_reason)
        .or_else(|| present(&body.rebase_in_progress).then_some(Attention::Rebase))
        .or_else(|| present(&body.merge_in_progress).then_some(Attention::Merge));
    GitStatusRead::Repository(RepositoryStatus {
        branch: body
            .current
            .map(|branch| branch.trim().to_owned())
            .filter(|branch| !branch.is_empty()),
        ahead: body.ahead,
        behind: body.behind,
        changes: Changes {
            files: body.files.len(),
            line_counts,
        },
        attention,
    })
}

/// Whether an optional object field is set: absent and `null` are not.
fn present(value: &Option<Value>) -> bool {
    value.as_ref().is_some_and(|value| !value.is_null())
}

/// Every action needs an ID, a name and a command; as in the web's schema,
/// one that lacks any makes the whole setup unreadable.
fn project_setup(body: ProjectSetupBody) -> Result<ProjectSetup, TransportError> {
    let actions = body
        .project_actions
        .into_iter()
        .map(project_action)
        .collect::<Result<Vec<_>, _>>()?;
    let untrusted = match (body.trust, body.shared) {
        (
            Some(TrustBody {
                hash: Some(hash),
                trusted: false,
            }),
            Some(shared),
        ) => Some(SharedTrust {
            hash,
            path: shared.path,
            setup_commands: shared.setup_worktree,
            actions: shared
                .project_actions
                .into_iter()
                .map(|action| (action.name, action.command))
                .collect(),
        }),
        _ => None,
    };
    Ok(ProjectSetup { actions, untrusted })
}

fn project_action(action: ActionBody) -> Result<ProjectAction, TransportError> {
    if action.id.is_empty() || action.name.is_empty() || action.command.is_empty() {
        return Err(TransportError::Malformed(
            "a project action lacks its ID, name or command".to_owned(),
        ));
    }
    Ok(ProjectAction {
        id: action.id,
        name: action.name,
        icon: action.icon,
        command: action.command,
        run_in_parent: action.run_in.as_deref() == Some("parent"),
        source: if action.source.as_deref() == Some("shared") {
            ActionSource::Shared
        } else {
            ActionSource::Personal
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_name_the_directory_and_the_path_derived_project() {
        let client = ServerClient::new(
            crate::endpoint::Endpoint::parse("http://127.0.0.1:4096/prefix").expect("valid"),
            crate::credential::Credential::None,
        )
        .expect("a client builds");
        let status = client.git_status_url("/work/my app");
        assert_eq!(status.path(), "/prefix/api/git/status");
        assert_eq!(status.query(), Some("directory=%2Fwork%2Fmy+app"));
        let project = ProjectId::from_path("/home/synthetic/project").expect("a path");
        assert_eq!(
            client.project_setup_url(&project).path(),
            "/prefix/api/projects/path_L2hvbWUvc3ludGhldGljL3Byb2plY3Q/config"
        );
    }

    fn read(json: &str) -> GitStatusRead {
        git_status_read(serde_json::from_str(json).expect("test JSON is valid"))
    }

    #[test]
    fn a_non_repository_answer_is_no_repository() {
        assert_eq!(
            read(r#"{"isGitRepository":false,"files":[],"branch":null,"ahead":0,"behind":0}"#),
            GitStatusRead::NotRepository
        );
    }

    #[test]
    fn a_status_sums_staged_and_working_lines() {
        let status = read(
            r#"{"current":" main ","tracking":null,"ahead":2,"behind":0,"isClean":false,
               "files":[{"path":"a"},{"path":"b"}],
               "diffStats":{"staged":{"a":{"insertions":3,"deletions":1}},"working":{"a":{"insertions":2,"deletions":0},"b":{"insertions":0,"deletions":4}}},
               "rebaseInProgress":{"headName":"x"}}"#,
        );
        assert_eq!(
            status,
            GitStatusRead::Repository(RepositoryStatus {
                branch: Some("main".to_owned()),
                ahead: 2,
                behind: 0,
                changes: Changes {
                    files: 2,
                    line_counts: Some(LineCounts {
                        additions: 5,
                        deletions: 5
                    }),
                },
                attention: Some(Attention::Rebase),
            })
        );
    }

    #[test]
    fn an_explicit_reason_wins_and_null_operations_are_none() {
        let GitStatusRead::Repository(status) =
            read(r#"{"current":"","files":[],"attentionReason":"bisect","mergeInProgress":null}"#)
        else {
            panic!("a repository");
        };
        assert_eq!(status.branch, None);
        assert_eq!(status.attention, Some(Attention::Bisect));
        assert_eq!(status.changes.line_counts, None);
    }

    #[test]
    fn actions_keep_their_order() {
        let body: ProjectSetupBody = serde_json::from_str(
            r#"{"projectActions":[{"id":"dev","name":"Dev","command":"bun dev","icon":"rocket","source":"shared"},{"id":"t","name":"Test","command":"bun test"}]}"#,
        )
        .expect("test JSON is valid");
        let setup = project_setup(body).expect("every action is complete");
        let actions = &setup.actions;
        assert_eq!(
            actions
                .iter()
                .map(|action| action.id.as_str())
                .collect::<Vec<_>>(),
            ["dev", "t"]
        );
        assert_eq!(actions[0].icon.as_deref(), Some("rocket"));
        assert_eq!(actions[0].source, ActionSource::Shared);
        assert_eq!(actions[1].source, ActionSource::Personal);
        assert_eq!(actions[0].command, "bun dev");
        assert_eq!(
            setup.untrusted, None,
            "a setup without trust state needs none"
        );
        let incomplete: ProjectSetupBody =
            serde_json::from_str(r#"{"projectActions":[{"id":"x","name":"X","command":""}]}"#)
                .expect("test JSON is valid");
        assert!(project_setup(incomplete).is_err());
    }

    #[test]
    fn untrusted_shared_commands_carry_what_the_prompt_shows() {
        let body: ProjectSetupBody = serde_json::from_str(
            r#"{"trust":{"hash":"h1","trusted":false},
               "projectActions":[{"id":"dev","name":"Dev","command":"bun dev","runIn":"parent","source":"shared"}],
               "shared":{"status":"ok","path":".openchamber/project.json","setupWorktree":["bun i"],"setupWorktreeWait":null,
                 "projectActions":[{"id":"dev","name":"Dev","command":"bun dev"}],"draftStarters":[],"plansDir":null}}"#,
        )
        .expect("test JSON is valid");
        let setup = project_setup(body).expect("valid");
        assert!(setup.actions[0].run_in_parent);
        assert_eq!(
            setup.untrusted,
            Some(SharedTrust {
                hash: "h1".to_owned(),
                path: ".openchamber/project.json".to_owned(),
                setup_commands: vec!["bun i".to_owned()],
                actions: vec![("Dev".to_owned(), "bun dev".to_owned())],
            })
        );
        let trusted: ProjectSetupBody = serde_json::from_str(
            r#"{"trust":{"hash":"h1","trusted":true},"projectActions":[],"shared":{"path":"p"}}"#,
        )
        .expect("test JSON is valid");
        assert_eq!(project_setup(trusted).expect("valid").untrusted, None);
    }
}
