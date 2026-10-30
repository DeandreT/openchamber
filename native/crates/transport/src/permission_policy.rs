//! Per-session permission modes, kept by OpenChamber's
//! `permission-auto-accept` runtime (`GET /api/permission-auto-accept`,
//! `PUT /api/permission-auto-accept/sessions/:id`), as
//! `stores/permissionStore.ts` reads and writes them.
//!
//! The answer to either route is the whole policy. The client applies that
//! snapshot; it never assumes a write took effect.

use std::collections::{BTreeMap, HashMap};

use openchamber_native_core::ids::SessionId;
use openchamber_native_core::permission_mode::{
    PermissionMode, PolicySnapshot, is_space_directory,
};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{ServerClient, encode_directory, read_json};
use crate::error::TransportError;

/// A policy as the server or its broadcast sends it. `modes` is the policy;
/// servers from before the modes send only `sessions`, where on means Auto.
#[derive(Deserialize)]
pub(crate) struct PolicyWire {
    sessions: HashMap<String, bool>,
    modes: Option<HashMap<String, ModeWire>>,
    revision: Option<u64>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ModeWire {
    Ask,
    Safety,
    Auto,
}

impl From<ModeWire> for PermissionMode {
    fn from(mode: ModeWire) -> Self {
        match mode {
            ModeWire::Ask => Self::Ask,
            ModeWire::Safety => Self::Safety,
            ModeWire::Auto => Self::Auto,
        }
    }
}

fn mode_wire(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => "ask",
        PermissionMode::Safety => "safety",
        PermissionMode::Auto => "auto",
    }
}

impl PolicyWire {
    /// The snapshot, or `None` when an entry names an empty session, as the
    /// web's schema rejects the whole policy.
    pub(crate) fn snapshot(self) -> Option<PolicySnapshot> {
        if self.sessions.keys().any(String::is_empty)
            || self
                .modes
                .as_ref()
                .is_some_and(|modes| modes.keys().any(String::is_empty))
        {
            return None;
        }
        let modes: BTreeMap<SessionId, PermissionMode> = match self.modes {
            Some(modes) => modes
                .into_iter()
                .map(|(id, mode)| (SessionId::new(id), mode.into()))
                .collect(),
            None => self
                .sessions
                .into_iter()
                .map(|(id, enabled)| {
                    let mode = if enabled {
                        PermissionMode::Auto
                    } else {
                        PermissionMode::Ask
                    };
                    (SessionId::new(id), mode)
                })
                .collect(),
        };
        Some(PolicySnapshot {
            modes,
            revision: self.revision,
        })
    }
}

/// Decodes a policy body or broadcast.
pub(crate) fn decode(value: Value) -> Result<PolicySnapshot, TransportError> {
    serde_json::from_value::<PolicyWire>(value)
        .ok()
        .and_then(PolicyWire::snapshot)
        .ok_or_else(|| TransportError::Malformed("invalid permission policy".to_owned()))
}

impl ServerClient {
    pub fn permission_policy_url(&self) -> Url {
        self.endpoint().url("api/permission-auto-accept")
    }

    pub fn permission_mode_url(&self, session: &SessionId) -> Url {
        let mut url = self.endpoint().url("api/permission-auto-accept/sessions/");
        url.path_segments_mut()
            .expect("http and https URLs have path segments")
            .pop_if_empty()
            .push(session.as_str());
        url
    }

    /// The host's permission policy. A failure is an error, never an empty
    /// policy: an empty one would show every session as Ask.
    pub async fn permission_policy(&self) -> Result<PolicySnapshot, TransportError> {
        let value: Value = self.get_json(self.permission_policy_url(), None).await?;
        decode(value)
    }

    /// Sets `session`'s mode and returns the server's whole policy after the
    /// write. `enabled` rides along for servers from before the modes; the
    /// directory lets the server reconcile requests already pending there.
    /// A space's directory also travels in the header that routes the write
    /// to that space's server, as `runtimeFetch` sends it; the answer is
    /// then the space's policy, not the host's.
    /// A lost response leaves the outcome unknown; the next read settles it.
    pub async fn set_permission_mode(
        &self,
        session: &SessionId,
        mode: PermissionMode,
        directory: Option<&str>,
    ) -> Result<PolicySnapshot, TransportError> {
        let mut body = json!({
            "mode": mode_wire(mode),
            "enabled": mode != PermissionMode::Ask,
        });
        if let Some(directory) = directory {
            body["directory"] = json!(directory);
        }
        let mut request = self
            .authorized(Method::PUT, self.permission_mode_url(session))
            .json(&body);
        if let Some(directory) = directory.filter(|directory| is_space_directory(directory)) {
            request = request
                .header("x-opencode-directory", encode_directory(directory))
                .header("x-opencode-directory-encoding", "uri");
        }
        let response = request.send().await.map_err(TransportError::from_request)?;
        let value: Value = read_json(response).await?;
        decode(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(json: &str) -> Option<PolicySnapshot> {
        decode(serde_json::from_str(json).expect("fixture is JSON")).ok()
    }

    #[test]
    fn modes_win_over_the_legacy_on_off_view() {
        let policy = snapshot(
            r#"{"sessions":{"a":true,"b":false},"modes":{"a":"safety","b":"ask"},"revision":4}"#,
        )
        .expect("valid policy");
        assert_eq!(policy.revision, Some(4));
        assert_eq!(
            policy.modes.get(&SessionId::from("a")),
            Some(&PermissionMode::Safety)
        );
    }

    #[test]
    fn legacy_enabled_values_map_to_auto_and_ask() {
        let policy = snapshot(r#"{"sessions":{"a":true,"b":false}}"#).expect("valid policy");
        assert_eq!(policy.revision, None);
        assert_eq!(
            policy.modes,
            [
                (SessionId::from("a"), PermissionMode::Auto),
                (SessionId::from("b"), PermissionMode::Ask),
            ]
            .into()
        );
    }

    #[test]
    fn malformed_policies_fail_instead_of_reading_as_empty() {
        for json in [
            "{}",
            r#"{"modes":{"a":"auto"}}"#,
            r#"{"sessions":{"a":"yes"}}"#,
            r#"{"sessions":{},"modes":{"a":"sometimes"}}"#,
            r#"{"sessions":{"":true}}"#,
            r#"{"sessions":{},"revision":-1}"#,
            r#"{"sessions":{},"revision":1.5}"#,
        ] {
            assert_eq!(snapshot(json), None, "{json} is rejected");
        }
    }

    #[test]
    fn mode_routes_encode_the_session_segment() {
        let client = ServerClient::new(
            crate::endpoint::Endpoint::parse("http://localhost:3000/prefix/")
                .expect("endpoint parses"),
            crate::credential::Credential::None,
        )
        .expect("client builds");
        assert_eq!(
            client
                .permission_mode_url(&SessionId::from("ses_a/b"))
                .path(),
            "/prefix/api/permission-auto-accept/sessions/ses_a%2Fb"
        );
        assert_eq!(
            client.permission_policy_url().path(),
            "/prefix/api/permission-auto-accept"
        );
    }
}
