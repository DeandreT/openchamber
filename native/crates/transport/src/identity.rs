//! Server identity and compatibility from `/health`.

use crate::error::TransportError;
use crate::wire;

/// The client API version this transport speaks.
pub const CLIENT_API_VERSION: u32 = 1;
/// The capability the global event bridge is announced by.
pub const GLOBAL_EVENTS_CAPABILITY: &str = "realtime.websocket.global-events.v1";
/// The capability of message pages with finished tool results left out.
pub const PART_SHELLS_CAPABILITY: &str = "api.message-part-shells.v1";

/// A verified OpenChamber server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerIdentity {
    pub openchamber_version: String,
    pub runtime: String,
    /// Stable across restarts of the same server, when it reports one.
    pub server_id: Option<String>,
    pub api_version: u32,
    pub capabilities: Vec<String>,
}

impl ServerIdentity {
    pub(crate) fn from_health(health: wire::Health) -> Result<Self, TransportError> {
        let (Some(version), Some(runtime)) = (health.openchamber_version, health.runtime) else {
            return Err(TransportError::NotOpenChamber);
        };
        if health.status.as_deref() != Some("ok") {
            return Err(TransportError::NotOpenChamber);
        }
        let compatibility = health.compatibility.ok_or_else(|| {
            TransportError::Incompatible("the server reports no API compatibility".to_owned())
        })?;
        if compatibility.min_client_api_version > CLIENT_API_VERSION {
            return Err(TransportError::Incompatible(format!(
                "the server needs client API {} or newer",
                compatibility.min_client_api_version
            )));
        }
        if compatibility.api_version < CLIENT_API_VERSION {
            return Err(TransportError::Incompatible(format!(
                "the server speaks API {}",
                compatibility.api_version
            )));
        }
        Ok(Self {
            openchamber_version: version,
            runtime,
            server_id: health.server_id,
            api_version: compatibility.api_version,
            capabilities: compatibility.capabilities,
        })
    }

    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|entry| entry == capability)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health(json: &str) -> wire::Health {
        serde_json::from_str(json).expect("test JSON is valid")
    }

    #[test]
    fn accepts_a_compatible_server() {
        let identity = ServerIdentity::from_health(health(
            r#"{"status":"ok","openchamberVersion":"2.4.0","runtime":"web","serverId":"abc",
                "compatibility":{"apiVersion":1,"minClientApiVersion":1,"capabilities":["realtime.websocket.global-events.v1"]}}"#,
        ))
        .expect("compatible");
        assert!(identity.supports(GLOBAL_EVENTS_CAPABILITY));
        assert_eq!(identity.server_id.as_deref(), Some("abc"));
    }

    #[test]
    fn distinguishes_foreign_and_incompatible_servers() {
        let foreign = ServerIdentity::from_health(health(r#"{"status":"ok"}"#));
        assert!(matches!(foreign, Err(TransportError::NotOpenChamber)));
        let newer = ServerIdentity::from_health(health(
            r#"{"status":"ok","openchamberVersion":"9.0.0","runtime":"web",
                "compatibility":{"apiVersion":3,"minClientApiVersion":2}}"#,
        ));
        assert!(matches!(newer, Err(TransportError::Incompatible(_))));
    }
}
