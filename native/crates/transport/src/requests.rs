//! Explicit permission and form operations against the captured OpenChamber client.
//!
//! No mutation is retried. A transport failure may follow application by the server;
//! callers reconcile the exact owner and ID before offering another explicit response.

use crate::client::{ServerClient, encode_directory};
use crate::error::TransportError;
use crate::request_wire;
use openchamber_native_core::form::{FormAnswer, FormDetail, FormRequest};
use openchamber_native_core::pending_requests::{RequestId, RequestKey};
use openchamber_native_core::permission::{PermissionReply, PermissionRequest};
use openchamber_native_core::request_list::{RequestList, RequestListIssue, RequestListIssueKind};
use openchamber_native_core::request_owner::{RequestOwner, RequestScope};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Declared request errors are distinct from unrelated 404s and unclassified HTTP errors.
/// Debug and Display never include answer text, metadata, or response bodies.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RequestError {
    #[error("invalid blocking request identity")]
    InvalidRequest,
    #[error("blocking request response is malformed")]
    InvalidResponse,
    #[error("blocking request response has a different owner or identity")]
    IdentityMismatch,
    #[error("blocking request response has a different location")]
    LocationMismatch,
    #[error("permission request no longer exists")]
    PermissionNotFound,
    #[error("form no longer exists")]
    FormNotFound,
    #[error("form is already settled")]
    FormAlreadySettled,
    #[error("the server refused the form answer")]
    FormInvalidAnswer,
    #[error("the owning session no longer exists")]
    SessionNotFound,
    #[error("the owning location is unavailable")]
    LocationNotFound,
    #[error(transparent)]
    Transport(#[from] TransportError),
}

#[derive(Deserialize)]
struct Data<T> {
    data: T,
}
#[derive(Deserialize)]
struct Located<T> {
    location: Location,
    data: T,
}
#[derive(Deserialize)]
struct Location {
    directory: String,
}
#[derive(Deserialize)]
struct DeclaredError {
    #[serde(rename = "_tag")]
    tag: String,
    #[serde(rename = "requestID")]
    request_id: Option<String>,
    id: Option<String>,
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
}

impl ServerClient {
    /// Reads a location list, preserving valid siblings and explicit entry issues.
    pub async fn pending_permissions(
        &self,
        scope: &RequestScope,
    ) -> Result<RequestList<PermissionRequest>, RequestError> {
        let wire: Located<Vec<Value>> = read(
            self.get_request(
                self.endpoint().url("api/permission/request"),
                Some(scope.directory()),
            ),
            None,
        )
        .await?;
        check_location(scope, &wire.location.directory)?;
        Ok(decode_list(
            wire.data,
            scope,
            None,
            request_wire::Permission::project,
            |p| p.id.as_str(),
            |p| RequestOwner::Session(p.session_id.clone()),
        ))
    }
    /// Reads location forms, including `global`, with explicit incomplete-entry issues.
    pub async fn pending_forms(
        &self,
        scope: &RequestScope,
    ) -> Result<RequestList<FormRequest>, RequestError> {
        let wire: Located<Vec<Value>> = read(
            self.get_request(self.endpoint().url("api/form"), Some(scope.directory())),
            None,
        )
        .await?;
        check_location(scope, &wire.location.directory)?;
        Ok(decode_list(
            wire.data,
            scope,
            None,
            request_wire::Form::project,
            |f| f.id.as_str(),
            |f| f.owner.clone(),
        ))
    }
    /// Reads a session's permissions with isolated entry failures; the server resolves its location.
    pub async fn session_permissions(
        &self,
        scope: &RequestScope,
        owner: &RequestOwner,
    ) -> Result<RequestList<PermissionRequest>, RequestError> {
        if !matches!(owner, RequestOwner::Session(_)) {
            return Err(RequestError::InvalidRequest);
        }
        let wire: Data<Vec<Value>> =
            read(self.list_request(scope, owner, "permission")?, None).await?;
        Ok(decode_list(
            wire.data,
            scope,
            Some(owner),
            request_wire::Permission::project,
            |p| p.id.as_str(),
            |p| RequestOwner::Session(p.session_id.clone()),
        ))
    }
    /// Reads pending forms for their actual owner. A location owner always sends its directory.
    pub async fn session_forms(
        &self,
        scope: &RequestScope,
        owner: &RequestOwner,
    ) -> Result<RequestList<FormRequest>, RequestError> {
        let wire: Data<Vec<Value>> = read(self.list_request(scope, owner, "form")?, None).await?;
        Ok(decode_list(
            wire.data,
            scope,
            Some(owner),
            request_wire::Form::project,
            |f| f.id.as_str(),
            |f| f.owner.clone(),
        ))
    }
    /// Reads one pending permission. Only its declared not-found error means it disappeared.
    pub async fn permission_request(
        &self,
        key: &RequestKey,
    ) -> Result<PermissionRequest, RequestError> {
        let RequestId::Permission(id) = &key.id else {
            return Err(RequestError::InvalidRequest);
        };
        let wire: Data<request_wire::Permission> =
            read(self.blocking_request(key, Method::GET, false)?, Some(key)).await?;
        let value = wire.data.project()?;
        if &value.id != id || value.session_id.as_str() != key.owner.as_str() {
            return Err(RequestError::IdentityMismatch);
        }
        Ok(value)
    }
    /// Reads pending or retained terminal state for one form, checking its owner and ID.
    pub async fn form_detail(&self, key: &RequestKey) -> Result<FormDetail, RequestError> {
        let RequestId::Form(id) = &key.id else {
            return Err(RequestError::InvalidRequest);
        };
        let wire: Data<request_wire::Detail> =
            read(self.blocking_request(key, Method::GET, false)?, Some(key)).await?;
        let value = wire.data.project()?;
        if &value.form.id != id || value.form.owner != key.owner {
            return Err(RequestError::IdentityMismatch);
        }
        Ok(value)
    }
    /// Sends exactly one explicit permission decision. An optional message changes reject semantics.
    pub async fn reply_permission(
        &self,
        key: &RequestKey,
        decision: PermissionReply,
        message: Option<&str>,
    ) -> Result<(), RequestError> {
        if !matches!(key.id, RequestId::Permission(_)) {
            return Err(RequestError::InvalidRequest);
        }
        let mut body = json!({"decision": decision.as_str()});
        if let Some(message) = message {
            body["message"] = message.into();
        }
        empty(
            self.blocking_request(key, Method::POST, true)?.json(&body),
            key,
        )
        .await
    }
    /// Sends one typed answer. The server validates JS regexp and format constraints.
    pub async fn reply_form(
        &self,
        key: &RequestKey,
        answer: &FormAnswer,
    ) -> Result<(), RequestError> {
        if !matches!(key.id, RequestId::Form(_)) {
            return Err(RequestError::InvalidRequest);
        }
        empty(
            self.blocking_request(key, Method::POST, true)?
                .json(&json!({"answer": request_wire::answer_wire(answer)})),
            key,
        )
        .await
    }
    /// Cancels one form. Omit the message for Electron's ordinary Cancel semantics.
    pub async fn cancel_form(
        &self,
        key: &RequestKey,
        message: Option<&str>,
    ) -> Result<(), RequestError> {
        if !matches!(key.id, RequestId::Form(_)) {
            return Err(RequestError::InvalidRequest);
        }
        empty(
            self.blocking_request_with_message(key, Method::DELETE, false, message)?,
            key,
        )
        .await
    }

    fn list_request(
        &self,
        scope: &RequestScope,
        owner: &RequestOwner,
        kind: &str,
    ) -> Result<RequestBuilder, RequestError> {
        check_owner(owner)?;
        let mut url = self.endpoint().url("api/session/");
        url.path_segments_mut()
            .expect("HTTP URL")
            .pop_if_empty()
            .push(owner.as_str())
            .push(kind);
        Ok(self.get_request(
            url,
            matches!(owner, RequestOwner::Location).then_some(scope.directory()),
        ))
    }
    pub(crate) fn blocking_request(
        &self,
        key: &RequestKey,
        method: Method,
        reply: bool,
    ) -> Result<RequestBuilder, RequestError> {
        self.blocking_request_with_message(key, method, reply, None)
    }
    fn blocking_request_with_message(
        &self,
        key: &RequestKey,
        method: Method,
        reply: bool,
        message: Option<&str>,
    ) -> Result<RequestBuilder, RequestError> {
        check_owner(&key.owner)?;
        let (kind, id) = match &key.id {
            RequestId::Permission(id)
                if matches!(key.owner, RequestOwner::Session(_))
                    && request_wire::valid_id(id.as_str(), "per") =>
            {
                ("permission", id.as_str())
            }
            RequestId::Form(id) if request_wire::valid_id(id.as_str(), "frm_") => {
                ("form", id.as_str())
            }
            _ => return Err(RequestError::InvalidRequest),
        };
        let mut url = self.endpoint().url("api/session/");
        {
            let mut path = url.path_segments_mut().expect("HTTP URL");
            path.pop_if_empty()
                .push(key.owner.as_str())
                .push(kind)
                .push(id);
            if reply {
                path.push("reply");
            }
        }
        if let Some(message) = message {
            url.query_pairs_mut().append_pair("message", message);
        }
        let request = self.authorized(method, url);
        Ok(if matches!(key.owner, RequestOwner::Location) {
            request.header(
                "x-opencode-directory",
                encode_directory(key.scope.directory()),
            )
        } else {
            request
        })
    }
}
fn check_owner(owner: &RequestOwner) -> Result<(), RequestError> {
    if RequestOwner::from_wire(owner.as_str()).as_ref() != Some(owner) {
        return Err(RequestError::InvalidRequest);
    }
    Ok(())
}
fn check_location(scope: &RequestScope, returned: &str) -> Result<(), RequestError> {
    let parsed = RequestScope::new(scope.runtime().clone(), returned)
        .ok_or(RequestError::LocationMismatch)?;
    if &parsed != scope {
        return Err(RequestError::LocationMismatch);
    }
    Ok(())
}
fn decode_list<W: DeserializeOwned, T>(
    raw: Vec<Value>,
    scope: &RequestScope,
    owner: Option<&RequestOwner>,
    project: fn(W) -> Result<T, RequestError>,
    id: fn(&T) -> &str,
    request_owner: fn(&T) -> RequestOwner,
) -> RequestList<T> {
    let mut issues = BTreeMap::new();
    let mut entries = Vec::new();
    let mut identities: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, value) in raw.into_iter().enumerate() {
        let parsed = serde_json::from_value::<W>(value)
            .map_err(|_| RequestError::InvalidResponse)
            .and_then(project);
        match parsed {
            Ok(entry) => {
                // Record identities only after full decoding. Invalid entries never supply guessed IDs.
                identities
                    .entry(id(&entry).to_owned())
                    .or_default()
                    .push(index);
                if owner.is_some_and(|owner| request_owner(&entry) != *owner) {
                    issues.insert(index, RequestListIssueKind::WrongOwner);
                } else {
                    entries.push((index, entry));
                }
            }
            Err(_) => {
                issues.insert(index, RequestListIssueKind::Malformed);
            }
        }
    }
    let mut duplicates = BTreeSet::new();
    for indices in identities.values().filter(|indices| indices.len() > 1) {
        for index in indices {
            duplicates.insert(*index);
            issues
                .entry(*index)
                .or_insert(RequestListIssueKind::DuplicateIdentity);
        }
    }
    let issues = issues
        .into_iter()
        .map(|(index, reason)| RequestListIssue { index, reason })
        .collect();
    RequestList::new(
        scope.clone(),
        owner.cloned(),
        entries
            .into_iter()
            .filter(|(index, _)| !duplicates.contains(index))
            .map(|(_, entry)| entry)
            .collect(),
        issues,
    )
}
async fn send(request: RequestBuilder, key: Option<&RequestKey>) -> Result<Response, RequestError> {
    let response = request.send().await.map_err(TransportError::from_request)?;
    let status = response.status();
    if status.is_redirection() {
        return Err(TransportError::Redirected.into());
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(TransportError::AuthenticationRequired.into());
    }
    if status.is_success() {
        return Ok(response);
    }
    let bytes = bounded_body(response).await?;
    if let Ok(error) = serde_json::from_slice::<DeclaredError>(&bytes) {
        let id_matches = |value: Option<&str>| {
            key.is_some_and(|key| {
                value
                    == Some(match &key.id {
                        RequestId::Permission(id) => id.as_str(),
                        RequestId::Form(id) => id.as_str(),
                    })
            })
        };
        let permission = key.is_some_and(|key| matches!(key.id, RequestId::Permission(_)));
        let form = key.is_some_and(|key| matches!(key.id, RequestId::Form(_)));
        let declared = match (status.as_u16(), error.tag.as_str()) {
            (404, "PermissionNotFoundError")
                if permission && id_matches(error.request_id.as_deref()) =>
            {
                Some(RequestError::PermissionNotFound)
            }
            (404, "FormNotFoundError") if form && id_matches(error.id.as_deref()) => {
                Some(RequestError::FormNotFound)
            }
            (409, "FormAlreadySettledError") if form && id_matches(error.id.as_deref()) => {
                Some(RequestError::FormAlreadySettled)
            }
            (400, "FormInvalidAnswerError") if form && id_matches(error.id.as_deref()) => {
                Some(RequestError::FormInvalidAnswer)
            }
            (404, "SessionNotFoundError")
                if key
                    .is_none_or(|key| error.session_id.as_deref() == Some(key.owner.as_str())) =>
            {
                Some(RequestError::SessionNotFound)
            }
            (404, "LocationNotFoundError") => Some(RequestError::LocationNotFound),
            _ => None,
        };
        if let Some(error) = declared {
            return Err(error);
        }
    }
    Err(TransportError::Http {
        status: status.as_u16(),
    }
    .into())
}
async fn bounded_body(mut response: Response) -> Result<Vec<u8>, RequestError> {
    const LIMIT: usize = 4 * 1024 * 1024;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(TransportError::from_request)?
    {
        if bytes.len().saturating_add(chunk.len()) > LIMIT {
            return Err(RequestError::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn read<T: DeserializeOwned>(
    request: RequestBuilder,
    key: Option<&RequestKey>,
) -> Result<T, RequestError> {
    let response = send(request, key).await?;
    if response.status() != StatusCode::OK
        || !response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(';').next() == Some("application/json"))
    {
        return Err(RequestError::InvalidResponse);
    }
    serde_json::from_slice(&bounded_body(response).await?)
        .map_err(|_| RequestError::InvalidResponse)
}
async fn empty(request: RequestBuilder, key: &RequestKey) -> Result<(), RequestError> {
    let response = send(request, Some(key)).await?;
    if response.status() != StatusCode::NO_CONTENT {
        return Err(RequestError::InvalidResponse);
    }
    Ok(())
}
