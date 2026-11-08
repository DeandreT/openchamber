//! Per-session warming, owned by the server (`lib/session-warming`): the
//! server keeps a chosen session's prompt cache warm after each turn. The
//! client reads the snapshot (`GET /api/session-warming`), changes one
//! session (`PUT /api/session-warming/sessions/:id`) and follows the
//! `openchamber:session-warming` broadcast, which carries the whole snapshot.

use openchamber_native_core::ids::SessionId;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{ServerClient, read_json};
use crate::error::TransportError;

/// The server's warming state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarmingSnapshot {
    /// Snapshot revisions only grow; JSON numbers keep their integer part.
    pub revision: i64,
    /// Settings chose per-session warming, so the toggles are offered.
    pub enabled: bool,
    /// How long a chosen session stays warm after a reply, in minutes.
    pub minutes: u32,
    /// The warmed sessions with the end of their warm window, `None` while
    /// a turn runs or before the first one ends.
    pub sessions: Vec<(SessionId, Option<i64>)>,
}

#[derive(Deserialize)]
struct SnapshotBody {
    revision: f64,
    enabled: bool,
    minutes: f64,
    sessions: Vec<SessionBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionBody {
    session_id: String,
    warm_until: Option<f64>,
}

impl SnapshotBody {
    fn into_snapshot(self) -> Option<WarmingSnapshot> {
        // `sessionWarmingSnapshotSchema` requires a non-empty session ID.
        let mut sessions = Vec::with_capacity(self.sessions.len());
        for entry in self.sessions {
            if entry.session_id.is_empty() {
                return None;
            }
            sessions.push((
                SessionId::from(entry.session_id),
                entry.warm_until.map(|until| until as i64),
            ));
        }
        Some(WarmingSnapshot {
            revision: self.revision as i64,
            enabled: self.enabled,
            minutes: self.minutes as u32,
            sessions,
        })
    }
}

/// Decodes a broadcast's properties; a malformed one changes nothing.
pub fn decode_snapshot(properties: &Value) -> Option<WarmingSnapshot> {
    serde_json::from_value::<SnapshotBody>(properties.clone())
        .ok()?
        .into_snapshot()
}

fn malformed() -> TransportError {
    TransportError::Malformed("unrecognized session warming snapshot".to_owned())
}

impl ServerClient {
    /// The server's warming snapshot.
    pub async fn session_warming(&self) -> Result<WarmingSnapshot, TransportError> {
        let body: SnapshotBody = self
            .get_json(self.endpoint().url("api/session-warming"), None)
            .await
            .map_err(|error| match error {
                TransportError::Malformed(_) => malformed(),
                other => other,
            })?;
        body.into_snapshot().ok_or_else(malformed)
    }

    /// Chooses how idle sessions stay warm: `off`, `all` (OpenCode's
    /// `warming` for every session) or `session` (the chosen ones).
    pub async fn set_warming_mode(&self, mode: &str) -> Result<WarmingSnapshot, TransportError> {
        self.put_warming("api/session-warming/mode", json!({ "mode": mode }))
            .await
    }

    /// Sets the warm duration of `scope` (`all` or `session`), in minutes.
    pub async fn set_warming_duration(
        &self,
        scope: &str,
        minutes: u32,
    ) -> Result<WarmingSnapshot, TransportError> {
        self.put_warming(
            "api/session-warming/duration",
            json!({ "scope": scope, "minutes": minutes }),
        )
        .await
    }

    async fn put_warming(
        &self,
        route: &str,
        body: Value,
    ) -> Result<WarmingSnapshot, TransportError> {
        let response = self
            .authorized(Method::PUT, self.endpoint().url(route))
            .json(&body)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let body: SnapshotBody = read_json(response).await.map_err(|error| match error {
            TransportError::Malformed(_) => malformed(),
            other => other,
        })?;
        body.into_snapshot().ok_or_else(malformed)
    }

    /// Warms a session of `directory` or stops warming it; the answer is
    /// the snapshot after the change.
    pub async fn set_session_warm(
        &self,
        id: &SessionId,
        directory: &str,
        warm: bool,
    ) -> Result<WarmingSnapshot, TransportError> {
        let mut url = self.endpoint().url("api/session-warming/sessions/");
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .pop_if_empty()
            .push(id.as_str());
        let response = self
            .authorized(Method::PUT, url)
            .json(&json!({ "directory": directory, "warm": warm }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let body: SnapshotBody = read_json(response).await.map_err(|error| match error {
            TransportError::Malformed(_) => malformed(),
            other => other,
        })?;
        body.into_snapshot().ok_or_else(malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcasts_decode_whole_snapshots_only() {
        let snapshot = decode_snapshot(&json!({
            "revision": 4,
            "enabled": true,
            "minutes": 60,
            "sessions": [{"sessionId": "ses_a", "warmUntil": null}, {"sessionId": "ses_b", "warmUntil": 9}]
        }))
        .expect("valid");
        assert!(snapshot.enabled);
        assert_eq!(snapshot.sessions.len(), 2);
        assert_eq!(snapshot.sessions[1].1, Some(9));
        assert!(
            decode_snapshot(&json!({"revision": 4, "enabled": true, "sessions": []})).is_none()
        );
        assert!(
            decode_snapshot(&json!({
                "revision": 4, "enabled": true, "minutes": 60,
                "sessions": [{"sessionId": "", "warmUntil": null}]
            }))
            .is_none()
        );
    }
}
