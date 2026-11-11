//! Settings → About (`components/update/openCodeUpgrade.ts`): whether the
//! server's OpenCode has an update (`GET /api/opencode/upgrade-status`) and
//! the install the server runs itself (`POST /api/opencode/upgrade`).

use std::time::Duration;

use reqwest::Method;
use serde::Deserialize;
use serde_json::json;

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::files::{self, read_answer};

/// How long an install may run before this client stops waiting for its
/// answer. An install downloads and replaces the OpenCode CLI, which outlasts
/// the ordinary request deadline; the server finishes it whether or not this
/// client still waits.
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// OpenCode on the connected server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenCodeUpgradeStatus {
    /// The installed version; `None` when the server could not read it.
    pub current_version: Option<String>,
    /// A newer version to announce; `None` when there is nothing to announce.
    pub available_version: Option<String>,
    /// Whether the server can run the upgrade itself.
    pub supported: bool,
}

/// Why an OpenCode upgrade did not report an install.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenCodeUpgradeError {
    /// The server answered that the install failed or declined it; `message`
    /// is its reason. Nothing was installed.
    #[error("the OpenCode upgrade failed")]
    Failed { message: Option<String> },
    /// No answer arrived. The install may still be running or may have
    /// finished: its outcome is unknown until the status is read again.
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl OpenCodeUpgradeError {
    /// The server's own reason, when it gave one.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Failed { message } => message.as_deref().filter(|message| !message.is_empty()),
            Self::Transport(_) => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusWire {
    available: Option<bool>,
    current_version: Option<String>,
    latest_version: Option<String>,
    upgrade: Option<UpgradeWire>,
}

#[derive(Deserialize)]
struct UpgradeWire {
    supported: Option<bool>,
}

#[derive(Default, Deserialize)]
struct InstalledWire {
    success: Option<bool>,
    version: Option<String>,
    error: Option<String>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

impl ServerClient {
    /// Reads the upgrade status. A failed check is an error and never reads
    /// as "no update".
    pub async fn opencode_upgrade_status(&self) -> Result<OpenCodeUpgradeStatus, TransportError> {
        let wire: StatusWire = self
            .get_json(self.endpoint().url("api/opencode/upgrade-status"), None)
            .await?;
        Ok(OpenCodeUpgradeStatus {
            current_version: non_empty(wire.current_version),
            // A version is announced only when the server says one is available.
            available_version: (wire.available == Some(true))
                .then(|| non_empty(wire.latest_version))
                .flatten(),
            supported: wire.upgrade.and_then(|upgrade| upgrade.supported) == Some(true),
        })
    }

    /// Installs the latest OpenCode on the server and returns the installed
    /// version when the server reports one. The request waits up to 30 minutes.
    /// A [`OpenCodeUpgradeError::Transport`] failure says nothing about the
    /// install itself.
    pub async fn upgrade_opencode(&self) -> Result<Option<String>, OpenCodeUpgradeError> {
        let response = self
            .authorized(Method::POST, self.endpoint().url("api/opencode/upgrade"))
            .timeout(UPGRADE_TIMEOUT)
            .json(&json!({}))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let bytes = match read_answer(response).await? {
            Ok(bytes) => bytes,
            Err(refusal) => {
                return Err(OpenCodeUpgradeError::Failed {
                    message: refusal.message,
                });
            }
        };
        let installed: InstalledWire = files::parse(&bytes).unwrap_or_default();
        if installed.success == Some(false) {
            return Err(OpenCodeUpgradeError::Failed {
                message: installed.error,
            });
        }
        Ok(installed.version)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[tokio::test]
    async fn a_version_is_announced_only_when_the_server_says_it_is_available() {
        let (client, _) =
            crate::test_server::serve(Box::new(|request| match request.method.as_str() {
                "GET" => (
                    200,
                    json!({
                        "available": false,
                        "currentVersion": " 2.2.0 ",
                        "latestVersion": "2.3.0",
                        "upgrade": {"supported": true},
                    }),
                ),
                _ => (200, json!({"success": false, "error": "npm is missing"})),
            }))
            .await;
        let status = client.opencode_upgrade_status().await.expect("status");
        assert_eq!(status.current_version.as_deref(), Some("2.2.0"));
        assert_eq!(status.available_version, None);
        assert!(status.supported);
        let failed = client.upgrade_opencode().await.expect_err("failed");
        assert_eq!(failed.message(), Some("npm is missing"));
    }

    #[tokio::test]
    async fn a_failed_check_is_an_error_and_an_install_reports_its_version() {
        let (client, _) =
            crate::test_server::serve(Box::new(|request| match request.method.as_str() {
                "GET" => (500, json!({"error": "down"})),
                _ => (200, json!({"success": true, "version": "2.3.0"})),
            }))
            .await;
        assert!(client.opencode_upgrade_status().await.is_err());
        assert_eq!(
            client
                .upgrade_opencode()
                .await
                .expect("installed")
                .as_deref(),
            Some("2.3.0")
        );
    }
}
