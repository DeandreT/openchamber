//! Settings → Commands (`useCommandsStore.ts`): the slash commands of one
//! directory with their stored configuration, read through OpenCode's command
//! list and `/api/config/commands/:name/config`, and written through
//! `/api/config/commands/:name`.

use futures_util::future;
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::client::{ServerClient, encode_directory};
use crate::error::TransportError;
use crate::files::read_answer;
use crate::wire;

/// The commands OpenCode provides itself (`BUILTIN_COMMAND_NAMES`).
const BUILT_IN_COMMANDS: [&str; 2] = ["init", "review"];

/// Where a command's file lives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandFileScope {
    /// The user's global configuration.
    #[default]
    User,
    Project,
}

/// One command with the configuration its own entry stores.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub description: Option<String>,
    pub agent: Option<String>,
    /// The joined `provider/model#variant` string.
    pub model: Option<String>,
    /// The markdown body of the command.
    pub template: Option<String>,
    /// Whether the command runs in a child session.
    pub subagent: bool,
    /// `None` when no file or config entry of the user defines the command.
    pub scope: Option<CommandFileScope>,
    /// The file the next save rewrites.
    pub path: Option<String>,
    /// The file still uses OpenCode 1 spellings.
    pub legacy: bool,
}

impl Command {
    /// Whether OpenCode itself provides the command (`isCommandBuiltIn`).
    pub fn is_built_in(&self) -> bool {
        BUILT_IN_COMMANDS.contains(&self.name.as_str())
    }
}

/// What a create or an update writes. An absent field is left as stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandChanges {
    pub description: Option<String>,
    /// `Some(None)` clears the agent override.
    pub agent: Option<Option<String>>,
    /// `Some(None)` clears the model override.
    pub model: Option<Option<String>>,
    pub template: Option<String>,
    pub subagent: Option<bool>,
}

impl CommandChanges {
    fn body(&self) -> Map<String, Value> {
        let mut body = Map::new();
        if let Some(description) = &self.description {
            body.insert("description".to_owned(), json!(description));
        }
        if let Some(agent) = &self.agent {
            body.insert("agent".to_owned(), json!(agent));
        }
        if let Some(model) = &self.model {
            body.insert("model".to_owned(), json!(model));
        }
        if let Some(template) = &self.template {
            body.insert("template".to_owned(), json!(template));
        }
        if let Some(subagent) = self.subagent {
            body.insert("subagent".to_owned(), json!(subagent));
        }
        body
    }
}

/// Why a command write did not land.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CommandError {
    #[error("the server refused the write (HTTP {status})")]
    Refused {
        status: u16,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

#[derive(Deserialize)]
struct CommandInfo {
    name: String,
    description: Option<String>,
}

/// `GET /api/config/commands/:name/config`.
#[derive(Deserialize)]
struct Envelope {
    source: Option<String>,
    scope: Option<String>,
    path: Option<String>,
    #[serde(default)]
    legacy: bool,
    #[serde(default)]
    config: StoredConfig,
}

#[derive(Default, Deserialize)]
struct StoredConfig {
    description: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    template: Option<String>,
    subagent: Option<bool>,
}

/// The provider/model pair and the variant of a joined model string
/// (`parseModelSelection`). `None` for text that names no model.
pub fn parse_model_selection(model: &str) -> Option<(String, Option<String>)> {
    let trimmed = model.trim();
    let (provider, rest) = trimmed.split_once('/')?;
    if provider.is_empty() || provider.contains('#') {
        return None;
    }
    let (model_id, variant) = match rest.split_once('#') {
        Some((model_id, variant)) => (model_id, Some(variant.trim())),
        None => (rest, None),
    };
    if model_id.is_empty() {
        return None;
    }
    Some((
        format!("{provider}/{model_id}"),
        variant
            .filter(|variant| !variant.is_empty())
            .map(str::to_owned),
    ))
}

/// Joins a `provider/model` pair and a variant (`formatModelSelection`).
/// `None` when `model` names no provider and model.
pub fn format_model_selection(model: &str, variant: &str) -> Option<String> {
    let (provider, model_id) = model.trim().split_once('/')?;
    let (provider, model_id) = (provider.trim(), model_id.trim());
    if provider.is_empty() || model_id.is_empty() {
        return None;
    }
    let variant = variant.trim();
    Some(if variant.is_empty() {
        format!("{provider}/{model_id}")
    } else {
        format!("{provider}/{model_id}#{variant}")
    })
}

impl ServerClient {
    fn command_url(&self, name: &str, config: bool, directory: Option<&str>) -> Url {
        let mut url = self.endpoint().url("api/config/commands/");
        {
            let mut segments = url
                .path_segments_mut()
                .expect("http and https URLs have path segments");
            segments.pop_if_empty().push(name);
            if config {
                segments.push("config");
            }
        }
        if let Some(directory) = directory {
            url.query_pairs_mut().append_pair("directory", directory);
        }
        url
    }

    /// The commands of `directory`, each with its stored configuration. A
    /// failed list is an error. A failed read of one command's entry keeps
    /// that command as OpenCode listed it and leaves the others complete.
    pub async fn commands(&self, directory: Option<&str>) -> Result<Vec<Command>, TransportError> {
        let listed: wire::Located<Vec<CommandInfo>> = self
            .get_json(self.opencode_url("command"), directory)
            .await?;
        let reads = listed.data.into_iter().map(|info| async move {
            let url = self.command_url(&info.name, true, directory);
            match self.get_json::<Envelope>(url, directory).await {
                Ok(envelope) => Command {
                    description: envelope.config.description.or(info.description),
                    agent: envelope.config.agent,
                    model: envelope.config.model,
                    template: envelope.config.template,
                    subagent: envelope.config.subagent == Some(true),
                    scope: match (envelope.source.as_deref(), envelope.scope.as_deref()) {
                        (Some("none"), _) => None,
                        (_, Some("project")) => Some(CommandFileScope::Project),
                        (_, Some("user")) => Some(CommandFileScope::User),
                        _ => None,
                    },
                    path: envelope.path,
                    legacy: envelope.legacy,
                    name: info.name,
                },
                Err(_) => Command {
                    name: info.name,
                    description: info.description,
                    ..Command::default()
                },
            }
        });
        Ok(future::join_all(reads).await)
    }

    /// The names of the skills `directory` has. OpenCode lists skills among
    /// its commands; the Commands page leaves them to the Skills page.
    pub async fn skill_names(
        &self,
        directory: Option<&str>,
    ) -> Result<Vec<String>, TransportError> {
        #[derive(Deserialize)]
        struct Skill {
            name: String,
        }
        #[derive(Deserialize)]
        struct Skills {
            #[serde(default)]
            skills: Vec<Skill>,
        }
        let mut url = self.endpoint().url("api/config/skills");
        if let Some(directory) = directory {
            url.query_pairs_mut().append_pair("directory", directory);
        }
        let listed: Skills = self.get_json(url, directory).await?;
        Ok(listed.skills.into_iter().map(|skill| skill.name).collect())
    }

    async fn command_write(
        &self,
        method: Method,
        name: &str,
        directory: Option<&str>,
        body: Option<Map<String, Value>>,
    ) -> Result<(), CommandError> {
        let mut request = self.authorized(method, self.command_url(name, false, directory));
        if let Some(directory) = directory {
            request = request.header("x-opencode-directory", encode_directory(directory));
        }
        if let Some(body) = body {
            request = request.json(&Value::Object(body));
        }
        let response = request.send().await.map_err(TransportError::from_request)?;
        match read_answer(response).await? {
            Ok(_) => Ok(()),
            Err(refusal) => Err(CommandError::Refused {
                status: refusal.status.as_u16(),
                message: refusal.message,
            }),
        }
    }

    /// Creates command `name` in `scope`. An empty description, agent or
    /// model is not written.
    pub async fn create_command(
        &self,
        name: &str,
        changes: &CommandChanges,
        scope: CommandFileScope,
        directory: Option<&str>,
    ) -> Result<(), CommandError> {
        let mut body = changes.body();
        body.retain(|_, value| !value.is_null() && value.as_str() != Some(""));
        body.entry("template").or_insert_with(|| json!(""));
        body.insert("scope".to_owned(), json!(scope));
        self.command_write(Method::POST, name, directory, Some(body))
            .await
    }

    /// Writes `changes` into command `name`'s file.
    pub async fn update_command(
        &self,
        name: &str,
        changes: &CommandChanges,
        directory: Option<&str>,
    ) -> Result<(), CommandError> {
        self.command_write(Method::PATCH, name, directory, Some(changes.body()))
            .await
    }

    /// Deletes command `name`, which resets a built-in one to its default.
    pub async fn delete_command(
        &self,
        name: &str,
        directory: Option<&str>,
    ) -> Result<(), CommandError> {
        self.command_write(Method::DELETE, name, directory, None)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_strings_split_and_join_like_the_config_writer() {
        assert_eq!(
            parse_model_selection(" anthropic/claude#high "),
            Some(("anthropic/claude".into(), Some("high".into())))
        );
        assert_eq!(
            parse_model_selection("openrouter/meta/llama"),
            Some(("openrouter/meta/llama".into(), None))
        );
        assert_eq!(parse_model_selection("no-provider"), None);
        assert_eq!(parse_model_selection("a#b/c"), None);
        assert_eq!(parse_model_selection("a/#v"), None);
        assert_eq!(
            format_model_selection("anthropic/claude", " max ").as_deref(),
            Some("anthropic/claude#max")
        );
        assert_eq!(
            format_model_selection("anthropic/claude", "").as_deref(),
            Some("anthropic/claude")
        );
        assert_eq!(format_model_selection("", "high"), None);
    }

    #[tokio::test]
    async fn one_unreadable_entry_keeps_the_other_commands_complete() {
        let (client, recorded) = crate::test_server::serve(Box::new(|request| {
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", path) if path.ends_with("/command") => (
                    200,
                    json!({"location": {}, "data": [
                        {"name": "review", "description": "Listed"},
                        {"name": "ship", "description": "Listed ship"},
                        {"name": "plugin"},
                    ]}),
                ),
                ("GET", "/api/config/commands/ship/config") => (
                    200,
                    json!({
                        "source": "md",
                        "scope": "project",
                        "path": "/repo/.opencode/command/ship.md",
                        "legacy": true,
                        "config": {
                            "agent": "build",
                            "model": "anthropic/claude#high",
                            "template": "Ship $ARGUMENTS",
                            "subagent": true,
                        },
                    }),
                ),
                ("GET", "/api/config/commands/plugin/config") => (
                    200,
                    json!({"source": "none", "scope": null, "path": null, "legacy": false, "config": {}}),
                ),
                ("GET", _) => (500, json!({"error": "unreadable"})),
                ("POST", _) => (200, json!({"success": true})),
                ("PATCH", _) => (400, json!({"error": "template is required"})),
                _ => (200, json!({"success": true})),
            }
        }))
        .await;
        let commands = client.commands(Some("/repo")).await.expect("list");
        assert_eq!(commands.len(), 3);
        assert!(commands[0].is_built_in());
        assert_eq!(
            commands[0].description.as_deref(),
            Some("Listed"),
            "a failed entry read keeps the command as listed"
        );
        assert_eq!(commands[1].scope, Some(CommandFileScope::Project));
        assert_eq!(commands[1].description.as_deref(), Some("Listed ship"));
        assert!(commands[1].subagent && commands[1].legacy);
        assert_eq!(commands[2].scope, None);

        let changes = CommandChanges {
            description: Some(String::new()),
            agent: Some(None),
            model: Some(Some("anthropic/claude".into())),
            template: Some("Body".into()),
            subagent: Some(false),
        };
        client
            .create_command("new", &changes, CommandFileScope::User, Some("/repo"))
            .await
            .expect("create");
        assert!(matches!(
            client.update_command("new", &changes, None).await,
            Err(CommandError::Refused {
                status: 400,
                message: Some(_)
            })
        ));
        client.delete_command("new", None).await.expect("delete");
        let recorded = recorded.lock().expect("log");
        let create = recorded
            .iter()
            .find(|request| request.method == "POST")
            .expect("create request");
        assert_eq!(create.path, "/api/config/commands/new");
        assert_eq!(
            create.body,
            json!({
                "model": "anthropic/claude",
                "template": "Body",
                "subagent": false,
                "scope": "user",
            }),
            "a create writes no empty description and no cleared agent"
        );
        let update = recorded
            .iter()
            .find(|request| request.method == "PATCH")
            .expect("update request");
        assert_eq!(
            update.body["agent"],
            Value::Null,
            "an update clears the agent override"
        );
    }

    #[tokio::test]
    async fn a_failed_list_is_an_error_and_never_an_empty_list() {
        let (client, _) =
            crate::test_server::serve(Box::new(|_| (500, json!({"error": "down"})))).await;
        assert!(client.commands(None).await.is_err());
    }
}
