//! Settings → Magic Prompts (`lib/magicPrompts.ts`): the overrides of one
//! scope, read and written through `/api/magic-prompts`.
//!
//! The global scope reads the global file alone. The project scope reads both
//! scopes of one registered project in one answer, so inherited text and the
//! project's own overrides come from the same read. Every write answers with
//! the whole view of its scope.

use std::collections::HashMap;

use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::json;

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::files::{self, read_answer};

/// One scope's overrides by prompt ID. Presence decides, so an empty text is
/// an override.
pub type MagicPromptOverrides = HashMap<String, String>;

/// The scope a Settings read or write addresses.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MagicPromptScope {
    /// The global file, whatever state a project file is in.
    Global,
    /// The registered project with this ID.
    Project(String),
}

/// One scope of a project view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MagicPromptScopeState {
    /// The scope was read; a missing file reads as no overrides.
    Readable(MagicPromptOverrides),
    /// The scope's file could not be read or parsed. It carries no overrides.
    Invalid { reason: String },
}

/// Both scopes of one registered project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MagicPromptProjectView {
    /// The project's directory.
    pub path: String,
    /// The project file's path below `path`.
    pub file: String,
    pub global: MagicPromptScopeState,
    pub project: MagicPromptScopeState,
}

/// What a scope last answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MagicPromptView {
    Global(MagicPromptOverrides),
    Project(MagicPromptProjectView),
}

/// Why a magic-prompt request did not answer with a view.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MagicPromptError {
    /// The server declined and said why.
    #[error("{message}")]
    Refused { status: u16, message: String },
    /// The answer described a project other than the one asked for.
    #[error("Magic prompts answered for another project")]
    WrongProject,
    #[error(transparent)]
    Transport(#[from] TransportError),
}

#[derive(Deserialize)]
struct GlobalWire {
    version: u32,
    overrides: MagicPromptOverrides,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum ScopeWire {
    Ok { overrides: MagicPromptOverrides },
    Missing { overrides: MagicPromptOverrides },
    Invalid { reason: String },
}

impl From<ScopeWire> for MagicPromptScopeState {
    fn from(wire: ScopeWire) -> Self {
        match wire {
            ScopeWire::Ok { overrides } | ScopeWire::Missing { overrides } => {
                Self::Readable(expand_legacy_keys(overrides))
            }
            ScopeWire::Invalid { reason } => Self::Invalid { reason },
        }
    }
}

#[derive(Deserialize)]
struct ProjectWire {
    id: String,
    path: String,
    file: String,
}

#[derive(Deserialize)]
struct ScopesWire {
    global: ScopeWire,
    project: ScopeWire,
}

#[derive(Deserialize)]
struct ProjectViewWire {
    version: u32,
    project: ProjectWire,
    scopes: ScopesWire,
}

/// The keys a scope written before the visible/instructions split stores as
/// one text (`LEGACY_PROMPT_KEY_MAP`).
const LEGACY_KEYS: [&str; 2] = ["git.commit.generate", "git.pr.generate"];

/// Splits each legacy text into its first line (the visible prompt) and the
/// rest (the instructions). A part fills only a split key the same scope does
/// not set itself (`expandLegacyKeys`).
fn expand_legacy_keys(mut overrides: MagicPromptOverrides) -> MagicPromptOverrides {
    for legacy in LEGACY_KEYS {
        let Some(text) = overrides.get(legacy).cloned() else {
            continue;
        };
        let (visible, instructions) = text.split_once('\n').unwrap_or((text.as_str(), ""));
        for (suffix, part) in [("visible", visible), ("instructions", instructions)] {
            let part = part.trim();
            let key = format!("{legacy}.{suffix}");
            if !part.is_empty() && !overrides.contains_key(&key) {
                overrides.insert(key, part.to_owned());
            }
        }
    }
    overrides
}

fn checked_version(version: u32) -> Result<(), MagicPromptError> {
    if version == 1 {
        Ok(())
    } else {
        Err(TransportError::Malformed("unknown magic prompts version".to_owned()).into())
    }
}

impl ServerClient {
    /// `/api/magic-prompts` or `/api/magic-prompts/:id`. A read of a project
    /// names only the project; a write names the scope as well.
    fn magic_prompt_scope_url(
        &self,
        id: Option<&str>,
        scope: &MagicPromptScope,
        writing: bool,
    ) -> Url {
        let mut url = self.endpoint().url("api/magic-prompts");
        if let Some(id) = id {
            url.path_segments_mut()
                .expect("http and https URLs have path segments")
                .push(id);
        }
        if let MagicPromptScope::Project(project) = scope {
            let mut query = url.query_pairs_mut();
            if writing {
                query.append_pair("scope", "project");
            }
            query.append_pair("projectId", project);
        }
        url
    }

    async fn magic_prompt_request(
        &self,
        method: Method,
        id: Option<&str>,
        scope: &MagicPromptScope,
        body: Option<serde_json::Value>,
    ) -> Result<MagicPromptView, MagicPromptError> {
        let writing = method != Method::GET;
        let fallback = match (&method, id) {
            (&Method::GET, _) => "Failed to load magic prompts",
            (&Method::PUT, _) => "Failed to save magic prompt",
            (_, Some(_)) => "Failed to reset magic prompt",
            (_, None) => "Failed to reset all magic prompts",
        };
        let mut request = self.authorized(method, self.magic_prompt_scope_url(id, scope, writing));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(TransportError::from_request)?;
        let bytes = match read_answer(response).await? {
            Ok(bytes) => bytes,
            Err(refusal) => {
                return Err(MagicPromptError::Refused {
                    status: refusal.status.as_u16(),
                    message: refusal
                        .message
                        .filter(|message| !message.is_empty())
                        .unwrap_or_else(|| fallback.to_owned()),
                });
            }
        };
        match scope {
            MagicPromptScope::Global => {
                let wire: GlobalWire = files::parse(&bytes)?;
                checked_version(wire.version)?;
                Ok(MagicPromptView::Global(expand_legacy_keys(wire.overrides)))
            }
            MagicPromptScope::Project(project) => {
                let wire: ProjectViewWire = files::parse(&bytes)?;
                checked_version(wire.version)?;
                if &wire.project.id != project {
                    return Err(MagicPromptError::WrongProject);
                }
                Ok(MagicPromptView::Project(MagicPromptProjectView {
                    path: wire.project.path,
                    file: wire.project.file,
                    global: wire.scopes.global.into(),
                    project: wire.scopes.project.into(),
                }))
            }
        }
    }

    /// Reads `scope` as the files are now. A failed or malformed read is an
    /// error and never an empty view.
    pub async fn magic_prompt_view(
        &self,
        scope: &MagicPromptScope,
    ) -> Result<MagicPromptView, MagicPromptError> {
        self.magic_prompt_request(Method::GET, None, scope, None)
            .await
    }

    /// Stores `text` as the override of prompt `id` in `scope`.
    pub async fn save_magic_prompt(
        &self,
        scope: &MagicPromptScope,
        id: &str,
        text: &str,
    ) -> Result<MagicPromptView, MagicPromptError> {
        self.magic_prompt_request(Method::PUT, Some(id), scope, Some(json!({ "text": text })))
            .await
    }

    /// Removes the override of prompt `id` from `scope`.
    pub async fn reset_magic_prompt(
        &self,
        scope: &MagicPromptScope,
        id: &str,
    ) -> Result<MagicPromptView, MagicPromptError> {
        self.magic_prompt_request(Method::DELETE, Some(id), scope, None)
            .await
    }

    /// Removes every override of `scope`.
    pub async fn reset_magic_prompts(
        &self,
        scope: &MagicPromptScope,
    ) -> Result<MagicPromptView, MagicPromptError> {
        self.magic_prompt_request(Method::DELETE, None, scope, None)
            .await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn overrides(pairs: &[(&str, &str)]) -> MagicPromptOverrides {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn a_legacy_text_fills_only_the_split_keys_the_scope_leaves_unset() {
        let expanded = expand_legacy_keys(overrides(&[
            (
                "git.commit.generate",
                " Visible line \nRule one\nRule two\n",
            ),
            ("git.pr.generate", "Only visible"),
            ("git.pr.generate.visible", "Own visible"),
        ]));
        assert_eq!(expanded["git.commit.generate.visible"], "Visible line");
        assert_eq!(
            expanded["git.commit.generate.instructions"],
            "Rule one\nRule two"
        );
        assert_eq!(
            expanded["git.pr.generate.visible"], "Own visible",
            "a split key the scope sets itself wins over the legacy text"
        );
        assert!(
            !expanded.contains_key("git.pr.generate.instructions"),
            "an empty part sets nothing"
        );
    }

    #[test]
    fn a_project_write_names_its_scope_and_a_read_names_only_the_project() {
        let client = ServerClient::new(
            crate::endpoint::Endpoint::parse("http://127.0.0.1:1/").expect("endpoint"),
            crate::credential::Credential::None,
        )
        .expect("client");
        let project = MagicPromptScope::Project("p 1".into());
        assert_eq!(
            client
                .magic_prompt_scope_url(None, &project, false)
                .as_str(),
            "http://127.0.0.1:1/api/magic-prompts?projectId=p+1"
        );
        assert_eq!(
            client
                .magic_prompt_scope_url(Some("plan.todo.visible"), &project, true)
                .as_str(),
            "http://127.0.0.1:1/api/magic-prompts/plan.todo.visible?scope=project&projectId=p+1"
        );
        assert_eq!(
            client
                .magic_prompt_scope_url(Some("plan.todo.visible"), &MagicPromptScope::Global, true)
                .as_str(),
            "http://127.0.0.1:1/api/magic-prompts/plan.todo.visible"
        );
    }

    fn project_body(id: &str, project: Value) -> Value {
        json!({
            "version": 1,
            "project": {"id": id, "path": "/work/app", "file": ".openchamber/project.json"},
            "scopes": {
                "global": {"status": "ok", "overrides": {"plan.todo.visible": "Global"}},
                "project": project,
            },
        })
    }

    #[tokio::test]
    async fn views_keep_an_invalid_scope_apart_from_an_empty_one() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", _) => (
                    200,
                    project_body("p1", json!({"status": "invalid", "reason": "bad JSON"})),
                ),
                ("PUT", "/api/magic-prompts/plan.todo.visible") => (
                    200,
                    project_body(
                        "p1",
                        json!({"status": "ok", "overrides": {"plan.todo.visible": "Mine"}}),
                    ),
                ),
                ("DELETE", "/api/magic-prompts/plan.todo.visible") => (
                    200,
                    project_body("other", json!({"status": "missing", "overrides": {}})),
                ),
                _ => (
                    409,
                    json!({"error": "The project file is invalid", "code": "invalid"}),
                ),
            }
        }))
        .await;
        let scope = MagicPromptScope::Project("p1".into());
        let MagicPromptView::Project(view) = client.magic_prompt_view(&scope).await.expect("read")
        else {
            panic!("a project read answers with a project view");
        };
        assert_eq!(
            view.project,
            MagicPromptScopeState::Invalid {
                reason: "bad JSON".into()
            }
        );
        assert_eq!(
            view.global,
            MagicPromptScopeState::Readable(overrides(&[("plan.todo.visible", "Global")]))
        );
        let saved = client
            .save_magic_prompt(&scope, "plan.todo.visible", "Mine")
            .await
            .expect("save");
        assert!(matches!(
            saved,
            MagicPromptView::Project(MagicPromptProjectView {
                project: MagicPromptScopeState::Readable(ref own),
                ..
            }) if own["plan.todo.visible"] == "Mine"
        ));
        assert!(
            matches!(
                client.reset_magic_prompt(&scope, "plan.todo.visible").await,
                Err(MagicPromptError::WrongProject)
            ),
            "an answer about another project is never shown"
        );
        let refused = client
            .reset_magic_prompts(&scope)
            .await
            .expect_err("refused");
        assert_eq!(refused.to_string(), "The project file is invalid");
        let recorded = recorded.lock().expect("log");
        assert_eq!(recorded[1].body, json!({"text": "Mine"}));
        assert_eq!(recorded[3].path, "/api/magic-prompts");
        assert_eq!(recorded[3].body, Value::Null);
    }

    #[tokio::test]
    async fn a_global_read_rejects_an_unknown_version_and_a_failure_without_a_reason() {
        let (client, _) =
            crate::test_server::serve(Box::new(|request| match request.method.as_str() {
                "GET" => (200, json!({"version": 2, "overrides": {}})),
                "PUT" => (200, json!({"version": 1, "overrides": {"a": 3}})),
                _ => (500, json!({})),
            }))
            .await;
        let scope = MagicPromptScope::Global;
        assert!(matches!(
            client.magic_prompt_view(&scope).await,
            Err(MagicPromptError::Transport(TransportError::Malformed(_)))
        ));
        assert!(
            matches!(
                client.save_magic_prompt(&scope, "a", "b").await,
                Err(MagicPromptError::Transport(TransportError::Malformed(_)))
            ),
            "an override that is not text fails the whole answer"
        );
        assert_eq!(
            client
                .reset_magic_prompts(&scope)
                .await
                .expect_err("failed")
                .to_string(),
            "Failed to reset all magic prompts"
        );
    }
}
