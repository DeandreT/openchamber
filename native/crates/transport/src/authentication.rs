//! Explicit authentication probes and password exchange for native clients.

use reqwest::header::{ACCEPT, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Method, Response, StatusCode};
use serde::Deserialize;

use crate::client::{ClientDescription, ServerClient};
use crate::credential::Credential;
use crate::error::TransportError;

const MAX_AUTH_BODY: usize = 16 * 1024;

/// The authentication decision returned by the selected server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthenticationStatus {
    Authenticated,
    PasswordRequired,
    /// Password entry cannot satisfy this server's pairing requirement.
    ClientTokenRequired,
}

/// Whether the user requested a trusted-device token or a short-lived sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginTrust {
    Session,
    Device,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusWire {
    authenticated: bool,
    locked: Option<bool>,
    #[serde(default)]
    client_auth_required: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginWire {
    authenticated: bool,
    client_token: Option<String>,
}

impl ServerClient {
    /// Probes the exact bearer credential that subsequent API calls will use.
    /// A failed or malformed response never grants authentication.
    pub async fn authentication_status(&self) -> Result<AuthenticationStatus, TransportError> {
        let response = self
            .authorized(Method::GET, self.endpoint().url("auth/session"))
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let status = response.status();
        let body = read_auth_body(response, status == StatusCode::UNAUTHORIZED).await?;
        let wire: StatusWire = serde_json::from_slice(&body)
            .map_err(|_| TransportError::Malformed("invalid authentication status".into()))?;
        match (
            status,
            wire.authenticated,
            wire.locked,
            wire.client_auth_required,
        ) {
            (StatusCode::OK, true, _, _) => Ok(AuthenticationStatus::Authenticated),
            (StatusCode::UNAUTHORIZED, false, Some(true), true) => {
                Ok(AuthenticationStatus::ClientTokenRequired)
            }
            (StatusCode::UNAUTHORIZED, false, Some(true), false) => {
                Ok(AuthenticationStatus::PasswordRequired)
            }
            _ => Err(TransportError::Malformed(
                "inconsistent authentication status".into(),
            )),
        }
    }

    /// Exchanges a password for a client token with the user's lifetime choice.
    /// The caller must verify the endpoint's identity before this operation.
    pub async fn login_with_trust(
        &self,
        password: &str,
        client: &ClientDescription,
        trust: LoginTrust,
    ) -> Result<Credential, TransportError> {
        let mut body = Self::login_body(password, client);
        body["trustDevice"] = serde_json::Value::Bool(trust == LoginTrust::Device);
        let response = self
            .authorized(Method::POST, self.endpoint().url("auth/session"))
            .header(ACCEPT, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(TransportError::AuthenticationRejected);
        }
        let bytes = read_auth_body(response, false).await?;
        let wire: LoginWire = serde_json::from_slice(&bytes)
            .map_err(|_| TransportError::Malformed("invalid sign-in response".into()))?;
        if !wire.authenticated {
            return Err(TransportError::AuthenticationRejected);
        }
        wire.client_token
            .and_then(Credential::from_client_token)
            .ok_or_else(|| {
                TransportError::Malformed("the login issued no valid client token".into())
            })
    }
}

async fn read_auth_body(
    mut response: Response,
    allow_unauthorized: bool,
) -> Result<Vec<u8>, TransportError> {
    let status = response.status();
    if status.is_redirection() {
        return Err(TransportError::Redirected);
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after_seconds = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok());
        return Err(TransportError::AuthenticationRateLimited {
            retry_after_seconds,
        });
    }
    if !status.is_success() && !(allow_unauthorized && status == StatusCode::UNAUTHORIZED) {
        return Err(TransportError::Http {
            status: status.as_u16(),
        });
    }
    if !response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
    {
        return Err(TransportError::Malformed(
            "authentication response is not JSON".into(),
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(TransportError::from_request)?
    {
        if body.len() + chunk.len() > MAX_AUTH_BODY {
            return Err(TransportError::Malformed(
                "authentication response exceeds its size limit".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
