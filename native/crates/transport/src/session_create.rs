//! Root-session creation and exact-ID recovery through the authenticated server.
//!
//! Callers must durably retain the request before dispatch. This module never
//! retries, deletes a session, or submits a prompt. A missing observation does
//! not prove that an earlier creation failed.

use openchamber_native_core::ids::SessionId;
use openchamber_native_core::path;
use openchamber_native_core::send_config::ModelChoice;
use openchamber_native_core::session::SessionRecord;
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};

use crate::client::ServerClient;
use crate::error::TransportError;
use crate::{projection, wire};

/// An immutable, validated root-session request. No parent can be supplied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateSession {
    id: SessionId,
    directory: String,
    model: Option<ModelChoice>,
    agent: Option<String>,
}

impl CreateSession {
    /// Captures the request once. Auto is omitted here; the caller must retain
    /// it in the first prompt and select it through OpenChamber before sending.
    pub fn new(
        id: SessionId,
        directory: &str,
        model: Option<ModelChoice>,
        agent: Option<String>,
    ) -> Result<Self, SessionError> {
        validate_id(&id)?;
        let directory = absolute_directory(directory)?;
        if model.as_ref().is_some_and(|model| {
            !valid_name(&model.provider_id)
                || !valid_name(&model.model_id)
                || model
                    .variant
                    .as_deref()
                    .is_some_and(|value| !valid_name(value))
        }) || agent.as_deref().is_some_and(|value| !valid_name(value))
        {
            return Err(SessionError::InvalidRequest);
        }
        Ok(Self {
            id,
            directory,
            model: model
                .filter(|model| model.provider_id != "openchamber" || model.model_id != "auto"),
            agent,
        })
    }

    /// The saved identity to use for every observation and explicit retry.
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// The normalized absolute remote directory captured before dispatch.
    pub fn directory(&self) -> &str {
        &self.directory
    }

    /// The concrete creation model, absent for defaults and Auto.
    pub fn model(&self) -> Option<&ModelChoice> {
        self.model.as_ref()
    }

    /// The captured creation agent, if explicitly supplied.
    pub fn agent(&self) -> Option<&str> {
        self.agent.as_deref()
    }
}

/// Errors contain no server response bodies or returned identity/path values.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("invalid session creation or observation request")]
    InvalidRequest,
    #[error("session response has a different identity")]
    IdentityMismatch,
    #[error("session response has a different or invalid directory")]
    LocationMismatch,
    #[error("session response is malformed")]
    InvalidResponse,
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// Whether a failed POST could have created the retained identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreationOutcome {
    /// The server explicitly refused the request.
    Rejected,
    /// Observe the exact ID before considering an explicit same-request retry.
    Unknown,
}

/// A failure retains the attempted identity and creation certainty.
#[derive(Debug, thiserror::Error)]
#[error("session creation failed: {source}")]
pub struct CreateError {
    pub session_id: SessionId,
    pub outcome: CreationOutcome,
    #[source]
    pub source: SessionError,
}

/// Read-only evidence. Absence never authorizes an automatic mutation.
#[derive(Debug)]
pub enum SessionObservation {
    Found(Box<SessionRecord>),
    NotObserved,
}

#[derive(Serialize)]
struct CreateBody<'a> {
    id: &'a str,
    location: Location<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<ModelRef<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<&'a str>,
}

#[derive(Serialize)]
struct Location<'a> {
    directory: &'a str,
}

#[derive(Serialize)]
struct ModelRef<'a> {
    #[serde(rename = "providerID")]
    provider_id: &'a str,
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    variant: Option<&'a str>,
}

#[derive(Deserialize)]
struct SessionResponse {
    data: wire::Session,
}

impl ServerClient {
    pub(crate) fn create_session_request(&self, request: &CreateSession) -> RequestBuilder {
        self.authorized(Method::POST, self.endpoint().url("api/session"))
            .json(&CreateBody {
                id: request.id.as_str(),
                location: Location {
                    directory: &request.directory,
                },
                model: request.model.as_ref().map(|model| ModelRef {
                    provider_id: &model.provider_id,
                    id: &model.model_id,
                    variant: model.variant.as_deref(),
                }),
                agent: request.agent.as_deref(),
            })
    }

    pub(crate) fn exact_session_request(&self, id: &SessionId) -> RequestBuilder {
        let mut url = self.endpoint().url("api/session/");
        url.path_segments_mut()
            .expect("HTTP URLs have path segments")
            .pop_if_empty()
            .push(id.as_str());
        self.authorized(Method::GET, url)
    }

    /// Creates once, validating identity and location before exposing the record.
    /// Dropping this future only cancels local I/O; the caller retains the ID.
    pub async fn create_session(
        &self,
        request: &CreateSession,
    ) -> Result<SessionRecord, CreateError> {
        let result = async {
            let response = self
                .create_session_request(request)
                .send()
                .await
                .map_err(TransportError::from_request)?;
            check_status(response.status())?;
            parse_session(response, &request.id, &request.directory).await
        }
        .await;
        result.map_err(|source| CreateError {
            session_id: request.id.clone(),
            outcome: match &source {
                SessionError::Transport(
                    TransportError::AuthenticationRequired
                    | TransportError::Http {
                        status: 400 | 401 | 403 | 404,
                    },
                ) => CreationOutcome::Rejected,
                _ => CreationOutcome::Unknown,
            },
            source,
        })
    }

    /// Reads exactly the saved identity, with no directory header or mutation.
    /// An unexplained canonical alias remains a location mismatch.
    pub async fn lookup_session(
        &self,
        id: &SessionId,
        directory: &str,
    ) -> Result<SessionObservation, SessionError> {
        validate_id(id)?;
        let directory = absolute_directory(directory)?;
        let response = self
            .exact_session_request(id)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(SessionObservation::NotObserved);
        }
        check_status(response.status())?;
        Ok(SessionObservation::Found(Box::new(
            parse_session(response, id, &directory).await?,
        )))
    }
}

fn check_status(status: StatusCode) -> Result<(), SessionError> {
    if status.is_redirection() {
        return Err(TransportError::Redirected.into());
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(TransportError::AuthenticationRequired.into());
    }
    if status != StatusCode::OK {
        return Err(TransportError::Http {
            status: status.as_u16(),
        }
        .into());
    }
    Ok(())
}

async fn parse_session(
    response: reqwest::Response,
    id: &SessionId,
    directory: &str,
) -> Result<SessionRecord, SessionError> {
    let reply: SessionResponse =
        crate::client::read_json(response)
            .await
            .map_err(|error| match error {
                TransportError::Malformed(_) => SessionError::InvalidResponse,
                other => SessionError::Transport(other),
            })?;
    verify_session(reply.data, id, directory)
}

fn verify_session(
    session: wire::Session,
    id: &SessionId,
    directory: &str,
) -> Result<SessionRecord, SessionError> {
    if session.id != id.as_str() {
        return Err(SessionError::IdentityMismatch);
    }
    let actual = absolute_directory(&session.location.directory)
        .map_err(|_| SessionError::LocationMismatch)?;
    if actual != directory {
        return Err(SessionError::LocationMismatch);
    }
    Ok(projection::session(session))
}

fn valid_name(value: &str) -> bool {
    !value.trim().is_empty() && !value.chars().any(char::is_control)
}

fn validate_id(id: &SessionId) -> Result<(), SessionError> {
    let value = id.as_str();
    if !value.starts_with("ses_")
        || value.len() <= 4
        || !value[4..].bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(SessionError::InvalidRequest);
    }
    Ok(())
}

// Remote paths must not be interpreted using the client's host filesystem.
fn absolute_directory(value: &str) -> Result<String, SessionError> {
    if value.trim() != value || value.chars().any(char::is_control) {
        return Err(SessionError::InvalidRequest);
    }
    let normalized = path::normalize(value).ok_or(SessionError::InvalidRequest)?;
    let bytes = normalized.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';
    if !(normalized.starts_with('/') || drive)
        || normalized.split('/').any(|part| matches!(part, "." | ".."))
        || (normalized.starts_with("//")
            && normalized[2..]
                .split('/')
                .filter(|part| !part.is_empty())
                .count()
                < 2)
    {
        return Err(SessionError::InvalidRequest);
    }
    Ok(normalized)
}

#[cfg(test)]
#[path = "session_create_tests.rs"]
mod tests;
