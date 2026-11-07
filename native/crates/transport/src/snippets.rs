//! Settings → Snippets (`useSnippetsStore.ts`): the snippet files a
//! directory can use, read and written through `/api/config/snippets`.

use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client::{ServerClient, encode_directory};
use crate::error::TransportError;
use crate::files::{self, read_answer};

/// Where a snippet file lives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SnippetScope {
    #[default]
    Global,
    Project,
}

/// One snippet as the server lists it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub file_path: String,
    pub source: SnippetScope,
}

/// The fields a create or an update sends.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SnippetFields {
    pub content: String,
    pub aliases: Vec<String>,
    pub description: String,
}

/// Why a snippet write did not land.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SnippetError {
    /// Another snippet already answers to the name (HTTP 409).
    #[error("the snippet name is taken")]
    NameTaken,
    #[error("the server refused the write (HTTP {status})")]
    Refused {
        status: u16,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

#[derive(Deserialize)]
struct Written {
    snippet: Option<Snippet>,
}

#[derive(Deserialize)]
struct Expanded {
    text: String,
}

impl ServerClient {
    fn snippet_url(&self, name: Option<&str>, directory: Option<&str>) -> Url {
        let mut url = self.endpoint().url("api/config/snippets");
        if let Some(name) = name {
            url.path_segments_mut()
                .expect("http and https URLs have path segments")
                .push(name);
        }
        if let Some(directory) = directory {
            url.query_pairs_mut().append_pair("directory", directory);
        }
        url
    }

    async fn snippet_write(
        &self,
        method: Method,
        url: Url,
        directory: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> Result<Vec<u8>, SnippetError> {
        let mut request = self.authorized(method, url);
        if let Some(directory) = directory {
            request = request.header("x-opencode-directory", encode_directory(directory));
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(TransportError::from_request)?;
        match read_answer(response).await? {
            Ok(bytes) => Ok(bytes),
            Err(refusal) if refusal.status.as_u16() == 409 => Err(SnippetError::NameTaken),
            Err(refusal) => Err(SnippetError::Refused {
                status: refusal.status.as_u16(),
                message: refusal.message,
            }),
        }
    }

    /// Every snippet `directory` can use, global and project.
    pub async fn snippets(&self, directory: Option<&str>) -> Result<Vec<Snippet>, TransportError> {
        self.get_json(self.snippet_url(None, directory), directory)
            .await
    }

    /// Creates `name` in `scope`.
    pub async fn create_snippet(
        &self,
        name: &str,
        fields: &SnippetFields,
        scope: SnippetScope,
        directory: Option<&str>,
    ) -> Result<(), SnippetError> {
        let mut body = serde_json::to_value(fields).expect("snippet fields serialize");
        body["scope"] = json!(scope);
        self.snippet_write(
            Method::POST,
            self.snippet_url(Some(name), directory),
            directory,
            Some(body),
        )
        .await?;
        Ok(())
    }

    /// Updates `name`; `rename` and `scope` move it to another file. Returns
    /// the snippet as saved, when the server says.
    pub async fn update_snippet(
        &self,
        name: &str,
        fields: &SnippetFields,
        rename: Option<&str>,
        scope: Option<SnippetScope>,
        directory: Option<&str>,
    ) -> Result<Option<Snippet>, SnippetError> {
        let mut body = serde_json::to_value(fields).expect("snippet fields serialize");
        if let Some(rename) = rename {
            body["name"] = json!(rename);
        }
        if let Some(scope) = scope {
            body["scope"] = json!(scope);
        }
        let bytes = self
            .snippet_write(
                Method::PATCH,
                self.snippet_url(Some(name), directory),
                directory,
                Some(body),
            )
            .await?;
        Ok(files::parse::<Written>(&bytes).ok().and_then(|written| written.snippet))
    }

    pub async fn delete_snippet(&self, name: &str, directory: Option<&str>) -> Result<(), SnippetError> {
        self.snippet_write(
            Method::DELETE,
            self.snippet_url(Some(name), directory),
            directory,
            None,
        )
        .await?;
        Ok(())
    }

    /// What a snippet body sends once used, with its own snippets expanded.
    pub async fn preview_snippet(
        &self,
        content: &str,
        directory: Option<&str>,
    ) -> Result<String, TransportError> {
        let mut url = self.endpoint().url("api/config/snippets/expand");
        if let Some(directory) = directory {
            url.query_pairs_mut().append_pair("directory", directory);
        }
        let bytes = self
            .snippet_write(Method::POST, url, directory, Some(json!({ "content": content })))
            .await
            .map_err(|error| match error {
                SnippetError::Transport(error) => error,
                SnippetError::NameTaken => TransportError::Http { status: 409 },
                SnippetError::Refused { status, .. } => TransportError::Http { status },
            })?;
        Ok(files::parse::<Expanded>(&bytes)?.text)
    }
}

/// The server's name pattern (`SNIPPET_NAME_PATTERN`): ASCII letters, digits,
/// `_` and `-`, starting with a letter or digit, at most 80 characters.
pub fn is_valid_snippet_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_alphanumeric())
        && name.len() <= 80
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// What a name field keeps of typed text: whitespace runs become a dash,
/// other characters drop, at most 80 characters (`normalizeSnippetNameInput`).
pub fn normalize_snippet_name_input(value: &str) -> String {
    let mut out = String::new();
    let mut in_space = false;
    for c in value.chars() {
        if c.is_whitespace() {
            if !in_space {
                out.push('-');
            }
            in_space = true;
            continue;
        }
        in_space = false;
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        }
    }
    out.chars().take(80).collect()
}

/// The snippet `#name` already triggers, by its name or an alias.
pub fn find_snippet_by_trigger<'a>(snippets: &'a [Snippet], name: &str) -> Option<&'a Snippet> {
    snippets.iter().find(|snippet| {
        snippet.name.eq_ignore_ascii_case(name)
            || snippet
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(name))
    })
}

/// Why `name` cannot be saved; `own` is the file being renamed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameProblem {
    Invalid,
    Taken,
}

pub fn snippet_name_problem(
    name: &str,
    snippets: &[Snippet],
    own: Option<&str>,
) -> Option<NameProblem> {
    if !is_valid_snippet_name(name) {
        return Some(NameProblem::Invalid);
    }
    find_snippet_by_trigger(snippets, name)
        .filter(|owner| Some(owner.file_path.as_str()) != own)
        .map(|_| NameProblem::Taken)
}

/// `base`, or `base-2`, `base-3`… until no snippet answers to it.
pub fn unique_snippet_name(base: &str, snippets: &[Snippet]) -> String {
    let mut name = base.to_owned();
    let mut counter = 2;
    while find_snippet_by_trigger(snippets, &name).is_some() {
        name = format!("{base}-{counter}");
        counter += 1;
    }
    name
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn snippet(name: &str, aliases: &[&str], path: &str) -> Snippet {
        Snippet {
            name: name.into(),
            content: "body".into(),
            aliases: aliases.iter().map(|alias| (*alias).into()).collect(),
            description: None,
            file_path: path.into(),
            source: SnippetScope::Global,
        }
    }

    #[test]
    fn names_follow_the_server_pattern() {
        assert!(is_valid_snippet_name("fix-it_2"));
        assert!(!is_valid_snippet_name("-lead"));
        assert!(!is_valid_snippet_name(""));
        assert!(!is_valid_snippet_name(&"a".repeat(81)));
        assert_eq!(normalize_snippet_name_input("my  big idea!"), "my-big-idea");
        let list = [snippet("review", &["rv"], "/a.md")];
        assert_eq!(
            snippet_name_problem("RV", &list, None),
            Some(NameProblem::Taken)
        );
        assert_eq!(
            snippet_name_problem("rv", &list, Some("/a.md")),
            None,
            "a snippet's own triggers stay free for it"
        );
        assert_eq!(unique_snippet_name("review", &list), "review-2");
    }

    #[tokio::test]
    async fn writes_carry_the_directory_and_a_taken_name_is_its_own_failure() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            match request.method.as_str() {
                "GET" if request.path.ends_with("/snippets") => (
                    200,
                    json!([{"name": "a", "content": "x", "aliases": [], "filePath": "/a.md", "source": "project"}]),
                ),
                "POST" if request.path.ends_with("/expand") => (200, json!({"text": "expanded"})),
                "POST" => (409, json!({"error": "already exists"})),
                "PATCH" => (
                    200,
                    json!({"success": true, "snippet": {"name": "b", "content": "x", "filePath": "/b.md", "source": "global"}}),
                ),
                _ => (500, json!({"error": "disk full"})),
            }
        }))
        .await;
        let list = client.snippets(Some("/work/app")).await.expect("list");
        assert_eq!(list[0].source, SnippetScope::Project);
        let fields = SnippetFields {
            content: "x".into(),
            ..SnippetFields::default()
        };
        assert!(matches!(
            client
                .create_snippet("a", &fields, SnippetScope::Project, Some("/work/app"))
                .await,
            Err(SnippetError::NameTaken)
        ));
        let saved = client
            .update_snippet("a", &fields, Some("b"), Some(SnippetScope::Global), None)
            .await
            .expect("update");
        assert_eq!(saved.map(|snippet| snippet.name).as_deref(), Some("b"));
        assert!(matches!(
            client.delete_snippet("b", None).await,
            Err(SnippetError::Refused { status: 500, message: Some(_) })
        ));
        assert_eq!(
            client.preview_snippet("#a", None).await.expect("preview"),
            "expanded"
        );
        let recorded = recorded.lock().expect("log");
        assert_eq!(recorded[1].path, "/api/config/snippets/a");
        assert_eq!(
            recorded[1].body,
            json!({"content": "x", "aliases": [], "description": "", "scope": "project"})
        );
        assert_eq!(recorded[2].body["name"], "b");
        assert_eq!(recorded[2].body["scope"], "global");
        assert_eq!(recorded[3].body, Value::Null);
    }
}
