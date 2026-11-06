//! The host's OpenCode compatibility check and its recovery requests, as
//! `lib/opencode/compatibility.ts` reads and sends them.

use reqwest::Method;
use serde::Deserialize;

use crate::client::{ServerClient, read_json};
use crate::error::TransportError;

/// Whether the host's OpenCode can run this version of OpenChamber.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompatibilityState {
    Compatible,
    Incompatible,
    /// The host could not tell.
    Unavailable,
}

/// Who installed the OpenCode the host runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Installation {
    Managed,
    External,
    Bundled,
}

/// The host's answer to `GET /api/opencode/compatibility`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenCodeCompatibility {
    pub state: CompatibilityState,
    pub version: Option<String>,
    pub installation: Installation,
    /// The local CLI that was checked; absent from older hosts.
    #[serde(default)]
    pub binary: Option<String>,
    /// Absent from hosts that predate the minimum-version gate.
    #[serde(default)]
    pub minimum_version: Option<String>,
    pub can_install: bool,
}

impl OpenCodeCompatibility {
    /// A 2.x below the minimum needs an update, not the move from v1.
    pub fn outdated(&self) -> bool {
        self.minimum_version.is_some()
            && self
                .version
                .as_deref()
                .is_some_and(|version| version.starts_with("2."))
    }
}

impl ServerClient {
    /// Reads the host's OpenCode compatibility.
    pub async fn opencode_compatibility(&self) -> Result<OpenCodeCompatibility, TransportError> {
        self.get_json(self.endpoint().url("api/opencode/compatibility"), None)
            .await
    }

    /// Asks the host to install OpenCode v2 and restart it.
    pub async fn install_opencode_v2(&self) -> Result<(), TransportError> {
        self.post_empty("api/opencode/install-v2").await
    }

    /// Asks the host to reload OpenCode's configuration, which restarts it.
    pub async fn reload_opencode(&self) -> Result<(), TransportError> {
        self.post_empty("api/config/reload").await
    }

    async fn post_empty(&self, path: &str) -> Result<(), TransportError> {
        let response = self
            .authorized(Method::POST, self.endpoint().url(path))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let _: serde_json::Value = read_json(response).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> OpenCodeCompatibility {
        serde_json::from_str(json).expect("a compatibility answer")
    }

    #[test]
    fn reads_current_and_older_host_answers() {
        let current = parse(
            r#"{"state":"incompatible","version":"2.0.1","installation":"external",
                "binary":null,"minimumVersion":"2.1.0","canInstall":false}"#,
        );
        assert_eq!(current.state, CompatibilityState::Incompatible);
        assert_eq!(current.installation, Installation::External);
        assert!(current.outdated());
        let older = parse(
            r#"{"state":"incompatible","version":"1.4.0","installation":"managed",
                "canInstall":true}"#,
        );
        assert_eq!(older.binary, None);
        assert_eq!(older.minimum_version, None);
        assert!(!older.outdated(), "v1 moves to v2 rather than updating");
        let v1_with_minimum = parse(
            r#"{"state":"incompatible","version":"1.4.0","installation":"managed",
                "minimumVersion":"2.1.0","canInstall":true}"#,
        );
        assert!(!v1_with_minimum.outdated());
    }

    #[test]
    fn rejects_unknown_states() {
        assert!(
            serde_json::from_str::<OpenCodeCompatibility>(
                r#"{"state":"maybe","version":null,"installation":"managed","canInstall":false}"#
            )
            .is_err()
        );
    }
}
