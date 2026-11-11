//! MCP servers of one directory: their connection status (`GET /api/mcp`),
//! OpenCode's connect and disconnect
//! (`POST /api/experimental/mcp/:server/connect` and `/disconnect`) and the
//! on/off switch that also writes the config file defining the server
//! (`GET /api/config/mcp`, `PATCH /api/config/mcp/:name`).
//!
//! Mirrors `listMcpServers`, `connectMcpServer` and `disconnectMcpServer` in
//! `packages/ui/src/lib/opencode/client.ts` and `setEnabled` in
//! `packages/ui/src/stores/useMcpStore.ts`. Every call names the directory
//! it works in: a read without one would start OpenCode's own working
//! directory and its MCP servers.

use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::json;

use crate::client::{ServerClient, encode_directory};
use crate::error::TransportError;
use crate::files::read_answer;
use crate::wire;

/// Where a server's connection stands (`McpServer.status.status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum McpConnection {
    Connected,
    /// OpenCode is still starting it; a status event follows.
    Pending,
    Disabled,
    Failed,
    /// It waits for a sign-in that OpenCode owns.
    NeedsAuth,
    /// A status this client does not know; it reads as not connected.
    Unrecognized,
}

impl McpConnection {
    fn from_wire(status: &str) -> Self {
        match status {
            "connected" => Self::Connected,
            "pending" => Self::Pending,
            "disabled" => Self::Disabled,
            "failed" => Self::Failed,
            "needs_auth" => Self::NeedsAuth,
            _ => Self::Unrecognized,
        }
    }
}

/// One MCP server of a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerStatus {
    pub name: String,
    pub connection: McpConnection,
}

#[derive(Deserialize)]
struct ServerBody {
    name: String,
    status: StatusBody,
}

#[derive(Deserialize)]
struct StatusBody {
    status: String,
}

/// One entry of `GET /api/config/mcp`: a server a config file defines.
#[derive(Deserialize)]
struct ConfigBody {
    name: String,
    /// Absent means the server is active.
    disabled: Option<bool>,
}

/// What switching a server on or off has to do (`setEnabled`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TogglePlan {
    /// The `disabled` value to write into the config file, when the file
    /// defines the server and says the opposite of what is wanted.
    pub write_disabled: Option<bool>,
    pub then: ToggleStep,
}

/// What follows the config write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToggleStep {
    /// The config already enables the server: switching on is a reconnect.
    Connect,
    /// Switching off also disconnects: a server started by hand while its
    /// config says off stays running through an unchanged file.
    Disconnect,
    /// The config was just switched on; OpenCode watches the file and
    /// starts the server itself.
    Nothing,
}

/// Plans the switch. `disabled_in_config` is `None` for a server no config
/// file defines, which can only be connected or disconnected.
pub(crate) fn toggle_plan(disabled_in_config: Option<bool>, enabled: bool) -> TogglePlan {
    let disabled = disabled_in_config == Some(true);
    TogglePlan {
        write_disabled: (disabled_in_config.is_some() && disabled == enabled).then_some(!enabled),
        then: match (enabled, disabled) {
            (true, false) => ToggleStep::Connect,
            (true, true) => ToggleStep::Nothing,
            (false, _) => ToggleStep::Disconnect,
        },
    }
}

impl ServerClient {
    fn mcp_server_url(&self, name: &str, operation: &str) -> Url {
        let mut url = self.opencode_url("experimental/mcp/");
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .pop_if_empty()
            .push(name)
            .push(operation);
        url
    }

    /// The URL of the MCP servers `directory`'s config files define, or of
    /// one of them.
    pub fn mcp_config_url(&self, name: Option<&str>, directory: &str) -> Url {
        let mut url = self.endpoint().url("api/config/mcp");
        if let Some(name) = name {
            url.path_segments_mut()
                .expect("HTTP URLs have path segments")
                .push(name);
        }
        url.query_pairs_mut().append_pair("directory", directory);
        url
    }

    /// The MCP servers of `directory` with their connection status, in the
    /// server's order. A failure is an error, never an empty list.
    pub async fn mcp_servers(
        &self,
        directory: &str,
    ) -> Result<Vec<McpServerStatus>, TransportError> {
        let servers: wire::Located<Vec<ServerBody>> = self
            .get_json(self.opencode_url("mcp"), Some(directory))
            .await?;
        Ok(servers
            .data
            .into_iter()
            .map(|server| McpServerStatus {
                connection: McpConnection::from_wire(&server.status.status),
                name: server.name,
            })
            .collect())
    }

    /// Asks OpenCode to connect `name` in `directory`.
    pub async fn connect_mcp_server(
        &self,
        name: &str,
        directory: &str,
    ) -> Result<(), TransportError> {
        self.post_mcp(self.mcp_server_url(name, "connect"), directory)
            .await
    }

    /// Asks OpenCode to disconnect `name` in `directory`. The disconnect
    /// lasts until OpenCode starts the directory again.
    pub async fn disconnect_mcp_server(
        &self,
        name: &str,
        directory: &str,
    ) -> Result<(), TransportError> {
        self.post_mcp(self.mcp_server_url(name, "disconnect"), directory)
            .await
    }

    async fn post_mcp(&self, url: Url, directory: &str) -> Result<(), TransportError> {
        let response = self
            .authorized(Method::POST, url)
            .header("x-opencode-directory", encode_directory(directory))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        settle(response).await
    }

    /// The on/off switch of `name` in `directory`. A configured server is
    /// switched in the config file that defines it, where OpenCode keeps the
    /// choice across restarts; the running server is then connected or
    /// disconnected to match. A server no config file defines can only be
    /// connected or disconnected.
    ///
    /// The steps run in order and the first failure ends them: without the
    /// config there is no telling whether a file keeps the server off, so a
    /// failed config read changes nothing.
    pub async fn set_mcp_server_enabled(
        &self,
        name: &str,
        enabled: bool,
        directory: &str,
    ) -> Result<(), TransportError> {
        let configs: Vec<ConfigBody> = self
            .get_json(self.mcp_config_url(None, directory), Some(directory))
            .await?;
        let configured = configs
            .iter()
            .find(|config| config.name == name)
            .map(|config| config.disabled == Some(true));
        let plan = toggle_plan(configured, enabled);
        if let Some(disabled) = plan.write_disabled {
            let response = self
                .authorized(Method::PATCH, self.mcp_config_url(Some(name), directory))
                .header("x-opencode-directory", encode_directory(directory))
                .json(&json!({ "disabled": disabled }))
                .send()
                .await
                .map_err(TransportError::from_request)?;
            settle(response).await?;
        }
        match plan.then {
            ToggleStep::Connect => self.connect_mcp_server(name, directory).await,
            ToggleStep::Disconnect => self.disconnect_mcp_server(name, directory).await,
            ToggleStep::Nothing => Ok(()),
        }
    }
}

/// A mutation's answer: any success, with or without a body.
async fn settle(response: reqwest::Response) -> Result<(), TransportError> {
    match read_answer(response).await? {
        Ok(_) => Ok(()),
        Err(refusal) => Err(TransportError::Http {
            status: refusal.status.as_u16(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn requests(recorded: &std::sync::Mutex<Vec<crate::test_server::Recorded>>) -> Vec<String> {
        recorded
            .lock()
            .expect("log")
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect()
    }

    #[test]
    fn the_switch_writes_config_only_when_the_file_says_the_opposite() {
        let plan = |configured, enabled| {
            let plan = toggle_plan(configured, enabled);
            (plan.write_disabled, plan.then)
        };
        // On: a disabled entry is switched on and OpenCode starts it.
        assert_eq!(plan(Some(true), true), (Some(false), ToggleStep::Nothing));
        // On: an enabled entry is a reconnect.
        assert_eq!(plan(Some(false), true), (None, ToggleStep::Connect));
        // Off: an enabled entry is switched off and disconnected.
        assert_eq!(
            plan(Some(false), false),
            (Some(true), ToggleStep::Disconnect)
        );
        // Off: an entry already off still disconnects a server started by hand.
        assert_eq!(plan(Some(true), false), (None, ToggleStep::Disconnect));
        // A server no file defines is only connected or disconnected.
        assert_eq!(plan(None, true), (None, ToggleStep::Connect));
        assert_eq!(plan(None, false), (None, ToggleStep::Disconnect));
    }

    #[tokio::test]
    async fn servers_read_their_status_and_failure_is_not_an_empty_list() {
        let (client, recorded) = crate::test_server::serve(Box::new(|_| {
            (
                200,
                json!({"location": {"directory": "/repo"}, "data": [
                    {"name": "zeta", "status": {"status": "connected"}},
                    {"name": "auth", "status": {"status": "needs_auth", "error": "sign in"}},
                    {"name": "new", "status": {"status": "warming"}},
                ]}),
            )
        }))
        .await;
        let servers = client.mcp_servers("/repo").await.expect("servers");
        assert_eq!(
            servers,
            [
                McpServerStatus {
                    name: "zeta".into(),
                    connection: McpConnection::Connected
                },
                McpServerStatus {
                    name: "auth".into(),
                    connection: McpConnection::NeedsAuth
                },
                McpServerStatus {
                    name: "new".into(),
                    connection: McpConnection::Unrecognized
                },
            ]
        );
        assert_eq!(requests(&recorded), ["GET /api/mcp"]);

        let (client, _) =
            crate::test_server::serve(Box::new(|_| (200, json!({"location": {}, "data": []}))))
                .await;
        assert!(client.mcp_servers("/repo").await.expect("empty").is_empty());

        let (client, _) =
            crate::test_server::serve(Box::new(|_| (503, json!({"error": "starting"})))).await;
        assert!(matches!(
            client.mcp_servers("/repo").await,
            Err(TransportError::Http { status: 503 })
        ));

        let (client, _) = crate::test_server::serve(Box::new(|_| {
            (200, json!({"data": [{"status": {"status": "connected"}}]}))
        }))
        .await;
        assert!(
            matches!(
                client.mcp_servers("/repo").await,
                Err(TransportError::Malformed(_))
            ),
            "a server without its name is not a shorter list"
        );
    }

    fn config_route(configs: Value, failing: &'static str) -> crate::test_server::Route {
        Box::new(move |request| {
            if !failing.is_empty() && request.path.ends_with(failing) {
                return (500, json!({"error": "boom"}));
            }
            if request.method == "GET" {
                (200, configs.clone())
            } else {
                (200, json!({"success": true}))
            }
        })
    }

    #[tokio::test]
    async fn switching_on_and_off_follows_the_config_then_the_running_server() {
        let configs = json!([
            {"name": "off", "type": "local", "command": ["x"], "disabled": true},
            {"name": "on", "type": "remote", "url": "https://mcp.test"},
        ]);

        let (client, recorded) = crate::test_server::serve(config_route(configs.clone(), "")).await;
        client
            .set_mcp_server_enabled("off", true, "/repo")
            .await
            .expect("enabled in config");
        assert_eq!(
            requests(&recorded),
            ["GET /api/config/mcp", "PATCH /api/config/mcp/off"],
            "OpenCode starts a server its file switches on"
        );
        assert_eq!(
            recorded.lock().expect("log")[1].body,
            json!({"disabled": false})
        );

        let (client, recorded) = crate::test_server::serve(config_route(configs.clone(), "")).await;
        client
            .set_mcp_server_enabled("on", false, "/repo")
            .await
            .expect("disabled");
        assert_eq!(
            requests(&recorded),
            [
                "GET /api/config/mcp",
                "PATCH /api/config/mcp/on",
                "POST /api/experimental/mcp/on/disconnect"
            ]
        );
        assert_eq!(
            recorded.lock().expect("log")[1].body,
            json!({"disabled": true})
        );

        let (client, recorded) = crate::test_server::serve(config_route(configs.clone(), "")).await;
        client
            .set_mcp_server_enabled("on", true, "/repo")
            .await
            .expect("reconnected");
        client
            .set_mcp_server_enabled("runtime only", false, "/repo")
            .await
            .expect("disconnected");
        assert_eq!(
            requests(&recorded),
            [
                "GET /api/config/mcp",
                "POST /api/experimental/mcp/on/connect",
                "GET /api/config/mcp",
                "POST /api/experimental/mcp/runtime%20only/disconnect"
            ],
            "an enabled entry reconnects and an unconfigured server writes no file"
        );
    }

    #[tokio::test]
    async fn the_first_failed_step_ends_the_switch() {
        let configs = json!([{"name": "on", "type": "local", "command": ["x"]}]);

        let (client, recorded) =
            crate::test_server::serve(config_route(configs.clone(), "/api/config/mcp")).await;
        assert!(matches!(
            client.set_mcp_server_enabled("on", false, "/repo").await,
            Err(TransportError::Http { status: 500 })
        ));
        assert_eq!(
            requests(&recorded),
            ["GET /api/config/mcp"],
            "without the config nothing is written or disconnected"
        );

        let (client, recorded) =
            crate::test_server::serve(config_route(configs.clone(), "/api/config/mcp/on")).await;
        assert!(
            client
                .set_mcp_server_enabled("on", false, "/repo")
                .await
                .is_err()
        );
        assert_eq!(
            requests(&recorded),
            ["GET /api/config/mcp", "PATCH /api/config/mcp/on"],
            "a failed config write does not disconnect"
        );

        let (client, _) = crate::test_server::serve(config_route(configs, "/connect")).await;
        assert!(matches!(
            client.set_mcp_server_enabled("on", true, "/repo").await,
            Err(TransportError::Http { status: 500 })
        ));
    }

    #[test]
    fn config_routes_carry_the_directory_as_a_query() {
        let endpoint = crate::endpoint::Endpoint::parse("http://127.0.0.1:1/").expect("endpoint");
        let client =
            ServerClient::new(endpoint, crate::credential::Credential::None).expect("client");
        assert_eq!(
            client.mcp_config_url(None, "/my repo").as_str(),
            "http://127.0.0.1:1/api/config/mcp?directory=%2Fmy+repo"
        );
        assert_eq!(
            client.mcp_config_url(Some("a/b"), "/repo").as_str(),
            "http://127.0.0.1:1/api/config/mcp/a%2Fb?directory=%2Frepo"
        );
    }
}
