//! Version readouts from OpenChamber-owned system endpoints.

use serde::Deserialize;

use crate::client::ServerClient;
use crate::error::TransportError;

/// Independent read results for the two version rows in About.
#[derive(Debug)]
pub struct SystemVersions {
    /// The selected host's application version; failure leaves the other read intact.
    pub openchamber: Result<String, TransportError>,
    /// The selected host's running OpenCode version, when available.
    pub opencode: Result<String, TransportError>,
}

#[derive(Deserialize)]
struct AppVersion {
    #[serde(rename = "openchamberVersion")]
    version: String,
}

#[derive(Deserialize)]
struct EngineVersion {
    #[serde(rename = "currentVersion")]
    version: String,
}

fn version(value: String) -> Result<String, TransportError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        return Err(TransportError::Malformed(
            "invalid version label".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

impl ServerClient {
    /// The selected host's OpenChamber version (`GET /api/system/info`).
    pub async fn openchamber_version(&self) -> Result<String, TransportError> {
        let reply: AppVersion = self
            .get_json(self.endpoint().url("api/system/info"), None)
            .await?;
        version(reply.version)
    }

    /// Reads both versions independently; either endpoint may be unavailable.
    pub async fn system_versions(&self) -> SystemVersions {
        let app = self.openchamber_version();
        let engine = async {
            let reply: EngineVersion = self
                .get_json(self.endpoint().url("api/opencode/upgrade-status"), None)
                .await?;
            version(reply.version)
        };
        let (openchamber, opencode) = tokio::join!(app, engine);
        SystemVersions {
            openchamber,
            opencode,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_labels_are_bounded_single_line_values() {
        for invalid in ["", "  ", "2.2\n0", &"x".repeat(129)] {
            assert!(super::version(invalid.to_owned()).is_err());
        }
        assert_eq!(
            super::version(" 2.2.0 ".to_owned()).expect("version"),
            "2.2.0"
        );
    }
}
