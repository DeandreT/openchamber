//! The composer pickers' catalogs that no send reads: the commands the `/`
//! palette lists (`useCommandsStore.loadCommands`) and the snippets the `#`
//! picker lists (`useSnippetsStore.loadSnippets`).

use futures_util::future;
use openchamber_native_core::autocomplete::{CommandScope, ServerCommand, SnippetItem};
use serde::Deserialize;

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::wire;

#[derive(Deserialize)]
struct CommandInfo {
    name: String,
    description: Option<String>,
}

/// `GET /api/config/commands/:name/config`: the command's own stored entry.
#[derive(Deserialize)]
struct CommandEnvelope {
    source: Option<String>,
    scope: Option<String>,
    #[serde(default)]
    config: CommandConfig,
}

#[derive(Default, Deserialize)]
struct CommandConfig {
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnippetWire {
    name: String,
    content: String,
    #[serde(default)]
    aliases: Vec<String>,
    description: Option<String>,
    source: String,
}

impl ServerClient {
    /// The commands of `directory`, each with the scope its config read
    /// found. A failed config read keeps the command as OpenCode listed it.
    pub async fn composer_commands(
        &self,
        directory: &str,
    ) -> Result<Vec<ServerCommand>, TransportError> {
        let listed: wire::Located<Vec<CommandInfo>> = self
            .get_json(self.opencode_url("command"), Some(directory))
            .await?;
        let reads = listed.data.into_iter().map(|command| async move {
            let mut url = self.endpoint().url("api/config/commands/");
            url.path_segments_mut()
                .expect("http and https URLs have path segments")
                .pop_if_empty()
                .extend([command.name.as_str(), "config"]);
            url.query_pairs_mut().append_pair("directory", directory);
            match self.get_json::<CommandEnvelope>(url, Some(directory)).await {
                Ok(envelope) => ServerCommand {
                    description: envelope.config.description.or(command.description),
                    scope: match envelope.scope.as_deref() {
                        Some("project") => Some(CommandScope::Project),
                        Some("user") => Some(CommandScope::User),
                        _ => None,
                    },
                    from_opencode: envelope.source.as_deref() == Some("none"),
                    name: command.name,
                },
                Err(_) => ServerCommand {
                    name: command.name,
                    description: command.description,
                    scope: None,
                    from_opencode: false,
                },
            }
        });
        Ok(future::join_all(reads).await)
    }

    /// The global and project snippets `directory` can use.
    pub async fn composer_snippets(
        &self,
        directory: &str,
    ) -> Result<Vec<SnippetItem>, TransportError> {
        let mut url = self.endpoint().url("api/config/snippets");
        url.query_pairs_mut().append_pair("directory", directory);
        let snippets: Vec<SnippetWire> = self.get_json(url, Some(directory)).await?;
        Ok(snippets
            .into_iter()
            .map(|snippet| SnippetItem {
                name: snippet.name,
                aliases: snippet.aliases,
                description: snippet.description,
                content: snippet.content,
                project: snippet.source == "project",
            })
            .collect())
    }
}
