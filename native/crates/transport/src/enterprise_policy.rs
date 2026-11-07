//! The enterprise policy the connected server enforces
//! (`GET /api/openchamber/enterprise-policy`, `publicEnterprisePolicy` in
//! `packages/web/server/lib/enterprise-mode.js`). The server refuses what the
//! policy forbids; this projection only lets the client hide those ways in and
//! say who decided.

use serde::Deserialize;

use crate::client::ServerClient;
use crate::error::TransportError;

/// The server's public policy, as `enterprisePolicySchema` reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerPolicy {
    pub enterprise_mode: bool,
    /// The administrator's organization, shown as who manages the app.
    pub organization: Option<String>,
    /// The policy file exists but could not be read; the mode is then on.
    pub policy_unreadable: bool,
    /// Network access stays closed; servers from before the rule never
    /// blocked it.
    pub network_access_blocked: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PolicyBody {
    enterprise_mode: bool,
    source: Option<String>,
    organization: Option<String>,
    policy_error: Option<String>,
    #[serde(default)]
    network_access_blocked: bool,
    #[serde(default)]
    opencode_binary: Option<String>,
}

impl PolicyBody {
    /// `null` or a non-empty string, as the schema's `min(1).nullable()`.
    fn into_policy(self) -> Option<ServerPolicy> {
        let present = |value: &Option<String>| value.as_ref().is_none_or(|text| !text.is_empty());
        if !present(&self.organization)
            || !present(&self.policy_error)
            || !present(&self.opencode_binary)
            || self
                .source
                .as_deref()
                .is_some_and(|source| !matches!(source, "policy-file" | "environment"))
        {
            return None;
        }
        Some(ServerPolicy {
            enterprise_mode: self.enterprise_mode,
            organization: self.organization,
            policy_unreadable: self.policy_error.is_some(),
            network_access_blocked: self.network_access_blocked,
        })
    }
}

/// Decodes the route's answer; anything the schema refuses is malformed.
fn decode(body: PolicyBody) -> Result<ServerPolicy, TransportError> {
    body.into_policy()
        .ok_or_else(|| TransportError::Malformed("unrecognized enterprise policy".to_owned()))
}

impl ServerClient {
    /// The server's policy, or `None` when the server predates the route
    /// (404). Any other failure is an error and never reads as "no policy".
    pub async fn enterprise_policy(&self) -> Result<Option<ServerPolicy>, TransportError> {
        match self
            .get_json::<PolicyBody>(
                self.endpoint().url("api/openchamber/enterprise-policy"),
                None,
            )
            .await
        {
            Ok(body) => decode(body).map(Some),
            Err(TransportError::Http { status: 404 }) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<ServerPolicy, TransportError> {
        decode(serde_json::from_str(json).expect("policy body"))
    }

    #[test]
    fn the_schema_decides_what_a_policy_is() {
        let policy = parse(
            r#"{"enterpriseMode":true,"source":"policy-file","organization":"Acme",
                "policyError":null,"networkAccessBlocked":true,"opencodeBinary":null}"#,
        )
        .expect("valid");
        assert_eq!(
            policy,
            ServerPolicy {
                enterprise_mode: true,
                organization: Some("Acme".to_owned()),
                policy_unreadable: false,
                network_access_blocked: true,
            }
        );
        // Servers from before the network rule and the binary pin omit both.
        let older = parse(
            r#"{"enterpriseMode":true,"source":"policy-file","organization":null,
                "policyError":"unreadable"}"#,
        )
        .expect("older server");
        assert!(older.policy_unreadable && !older.network_access_blocked);
        for refused in [
            r#"{"enterpriseMode":false,"source":null,"organization":"","policyError":null}"#,
            r#"{"enterpriseMode":false,"source":"user","organization":null,"policyError":null}"#,
            r#"{"enterpriseMode":false,"source":null,"organization":null,"policyError":""}"#,
        ] {
            assert!(parse(refused).is_err(), "{refused}");
        }
        assert!(serde_json::from_str::<PolicyBody>(r#"{"enterpriseMode":"yes"}"#).is_err());
    }
}
