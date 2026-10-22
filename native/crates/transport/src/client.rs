//! Requests to one OpenChamber server.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use openchamber_native_core::chat_roots::ChatRoots;
use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::ownership::{self, ConfiguredProject, WorktreeInfo};
use openchamber_native_core::session::{LiveStatus, SessionRecord};
use openchamber_native_core::transcript::Message;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Method, RequestBuilder, Response, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::credential::Credential;
use crate::endpoint::Endpoint;
use crate::error::TransportError;
use crate::identity::{PART_SHELLS_CAPABILITY, ServerIdentity};
use crate::projection;
use crate::wire;

/// Page size of global session reads, as the web client uses.
pub const SESSION_PAGE_SIZE: usize = 500;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Asks for message pages with finished tool results left out
/// (`packages/web/server/lib/opencode/message-shells.js`).
const PART_SHELLS_HEADER: &str = "x-openchamber-part-shells";
const PART_SHELLS_VERSION: &str = "v1";
/// The settings surface this client reads and writes.
const SURFACE: &str = "desktop";
/// Directories per worktree request; the server caps a batch.
const WORKTREE_BATCH_SIZE: usize = 200;
/// Label stored with a client token the server issues to this app.
const CLIENT_LABEL: &str = "OpenChamber Native";

/// One page of sessions and the cursor of the next.
#[derive(Debug)]
pub struct SessionPage {
    pub sessions: Vec<SessionRecord>,
    pub next: Option<String>,
}

/// One project's worktree read.
#[derive(Debug, PartialEq, Eq)]
pub struct WorktreeRead {
    pub project_path: String,
    /// `None` when the server could not list this project; earlier topology
    /// for it stays.
    pub worktrees: Option<Vec<WorktreeInfo>>,
}

/// The pages of the global session list, newest first, without duplicates.
///
/// Paging stops when the server names no further cursor, repeats one, or
/// returns a page with nothing new, so a misbehaving cursor cannot spin.
#[derive(Debug)]
pub struct SessionPages {
    client: ServerClient,
    cursor: Option<String>,
    seen: HashSet<SessionId>,
    done: bool,
}

impl SessionPages {
    /// The next page's new sessions, or `None` after the last page.
    pub async fn next(&mut self) -> Result<Option<Vec<SessionRecord>>, TransportError> {
        if self.done {
            return Ok(None);
        }
        let page = self.client.session_page(self.cursor.as_deref()).await?;
        let mut accepted = Vec::with_capacity(page.sessions.len());
        for session in page.sessions {
            if self.seen.insert(session.id.clone()) {
                accepted.push(session);
            }
        }
        match page.next {
            Some(next) if !accepted.is_empty() && self.cursor.as_deref() != Some(next.as_str()) => {
                self.cursor = Some(next);
            }
            _ => self.done = true,
        }
        Ok(Some(accepted))
    }
}

/// One page of a session's messages, oldest first.
#[derive(Debug)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    /// The cursor of the next older page.
    pub older: Option<String>,
}

/// A server connection with its credential.
///
/// The credential stays inside the client; it is sent only in the
/// `Authorization` header and never follows a redirect.
#[derive(Clone, Debug)]
pub struct ServerClient {
    http: reqwest::Client,
    endpoint: Endpoint,
    credential: Credential,
    /// Whether message pages are requested with finished tool results left out.
    part_shells: bool,
}

impl ServerClient {
    pub fn new(endpoint: Endpoint, credential: Credential) -> Result<Self, TransportError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(TransportError::from_request)?;
        Ok(Self {
            http,
            endpoint,
            credential,
            part_shells: false,
        })
    }

    /// Adopts the capabilities the server announced in its identity.
    pub fn with_identity(mut self, identity: &ServerIdentity) -> Self {
        self.part_shells = identity.supports(PART_SHELLS_CAPABILITY);
        self
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    /// Verifies the service identity without sending credentials.
    pub async fn probe(&self) -> Result<ServerIdentity, TransportError> {
        let response = self
            .http
            .get(self.endpoint.url("health"))
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let health: wire::Health = match read_json(response).await {
            Ok(health) => health,
            Err(TransportError::Malformed(_) | TransportError::Http { .. }) => {
                return Err(TransportError::NotOpenChamber);
            }
            Err(error) => return Err(error),
        };
        ServerIdentity::from_health(health)
    }

    /// Exchanges the UI password for a trusted-device client token.
    ///
    /// The password is sent once in the request body and not retained.
    pub async fn login(&self, password: &str) -> Result<Credential, TransportError> {
        let response = self
            .http
            .post(self.endpoint.url("auth/session"))
            .header(ACCEPT, "application/json")
            .json(&json!({
                "password": password,
                "trustDevice": true,
                "issueClientToken": true,
                "label": CLIENT_LABEL,
            }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(TransportError::AuthenticationRejected);
        }
        let login: wire::SessionLogin = read_json(response).await?;
        login
            .client_token
            .filter(|token| token.starts_with("oc_client_"))
            .map(Credential::ClientToken)
            .ok_or_else(|| TransportError::Malformed("the login issued no client token".to_owned()))
    }

    /// The URL of one global session page.
    pub fn session_page_url(&self, cursor: Option<&str>) -> Url {
        let mut url = self.endpoint.url("api/session");
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("limit", &SESSION_PAGE_SIZE.to_string());
            if let Some(cursor) = cursor {
                pairs.append_pair("cursor", cursor);
            }
        }
        url
    }

    /// The URL of a session's newest `limit` messages.
    pub fn messages_url(&self, session_id: &SessionId, limit: usize) -> Url {
        let mut url = self.endpoint.url("api/session/");
        url.path_segments_mut()
            .expect("http and https URLs have path segments")
            .pop_if_empty()
            .extend([session_id.as_str(), "message"]);
        url.query_pairs_mut()
            .append_pair("limit", &limit.to_string());
        url
    }

    /// One page of every session the server knows, newest first.
    pub async fn session_page(&self, cursor: Option<&str>) -> Result<SessionPage, TransportError> {
        let page: wire::Page<wire::Session> =
            self.get_json(self.session_page_url(cursor), None).await?;
        Ok(SessionPage {
            sessions: page.data.into_iter().map(projection::session).collect(),
            next: page.cursor.next,
        })
    }

    /// Every session, walking pages until the list stops growing.
    ///
    /// A failed page fails the whole read: a partial list has no authority
    /// to remove sessions it did not reach.
    pub async fn all_sessions(&self) -> Result<Vec<SessionRecord>, TransportError> {
        let mut pages = self.session_pages();
        let mut sessions = Vec::new();
        while let Some(page) = pages.next().await? {
            sessions.extend(page);
        }
        Ok(sessions)
    }

    /// Pages of every session, for a caller that shows the first page early.
    pub fn session_pages(&self) -> SessionPages {
        SessionPages {
            client: self.clone(),
            cursor: None,
            seen: HashSet::new(),
            done: false,
        }
    }

    /// The newest page of a session's messages, returned oldest first.
    ///
    /// Session routes resolve the session's own location, so no directory
    /// header is sent.
    ///
    /// When the server supports it, finished tool results come as shells
    /// (`PartContent::Tool::elided`); `session_message` completes one.
    pub async fn latest_messages(
        &self,
        session_id: &SessionId,
        limit: usize,
    ) -> Result<MessagePage, TransportError> {
        let response = self
            .latest_messages_request(session_id, limit)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        message_page(read_json(response).await?, limit)
    }

    pub(crate) fn latest_messages_request(
        &self,
        session_id: &SessionId,
        limit: usize,
    ) -> RequestBuilder {
        let request = self.authorized(Method::GET, self.messages_url(session_id, limit));
        if self.part_shells {
            request.header(PART_SHELLS_HEADER, PART_SHELLS_VERSION)
        } else {
            request
        }
    }

    /// The URL of one whole message.
    pub fn session_message_url(&self, session_id: &SessionId, message_id: &MessageId) -> Url {
        let mut url = self.endpoint.url("api/session/");
        url.path_segments_mut()
            .expect("http and https URLs have path segments")
            .pop_if_empty()
            .extend([session_id.as_str(), "message", message_id.as_str()]);
        url
    }

    /// One whole message, with every tool result a page left out.
    pub async fn session_message(
        &self,
        session_id: &SessionId,
        message_id: &MessageId,
    ) -> Result<Message, TransportError> {
        let single: wire::Single<wire::MessageEnvelope> = self
            .get_json(self.session_message_url(session_id, message_id), None)
            .await?;
        projection::message(single.data)?.ok_or_else(|| {
            TransportError::Malformed("the message has a type this client does not know".to_owned())
        })
    }

    /// The URL of the settings document as this client's surface reads it.
    pub fn settings_url(&self) -> Url {
        let mut url = self.endpoint.url("api/config/settings");
        // A surface header forces a CORS preflight older servers refuse; the
        // server reads the surface from the query.
        url.query_pairs_mut().append_pair("surface", SURFACE);
        url
    }

    /// The projects registered in OpenChamber.
    pub async fn projects(&self) -> Result<Vec<ConfiguredProject>, TransportError> {
        let settings: wire::Settings = self.get_json(self.settings_url(), None).await?;
        let paths = settings
            .projects
            .iter()
            .filter_map(|entry| entry.get("path").and_then(Value::as_str));
        Ok(ownership::configured_projects(
            paths,
            settings.home_directory.as_deref(),
        ))
    }

    /// The URL of one worktree batch; `first` routes it as the UI does.
    pub fn worktree_batch_url(&self, first: &str) -> Url {
        let mut url = self.endpoint.url("api/git/project-worktrees");
        url.query_pairs_mut().append_pair("directory", first);
        url
    }

    /// The linked worktrees of each project, in batches.
    ///
    /// A failed batch fails the read; a directory the server could not list
    /// is reported on its own, so it never erases another project's
    /// worktrees.
    pub async fn project_worktrees(
        &self,
        project_paths: &[String],
    ) -> Result<Vec<WorktreeRead>, TransportError> {
        let mut reads = Vec::with_capacity(project_paths.len());
        for chunk in project_paths.chunks(WORKTREE_BATCH_SIZE) {
            let response = self
                .authorized(Method::POST, self.worktree_batch_url(&chunk[0]))
                .json(&json!({ "directories": chunk }))
                .send()
                .await
                .map_err(TransportError::from_request)?;
            let batch: wire::WorktreeBatch = read_json(response).await?;
            let mut by_directory: HashMap<String, Option<Vec<wire::WorktreeEntry>>> = batch
                .projects
                .into_iter()
                .map(|entry| (entry.directory, entry.worktrees))
                .collect();
            for project_path in chunk {
                let listing = by_directory.remove(project_path).flatten();
                reads.push(WorktreeRead {
                    project_path: project_path.clone(),
                    worktrees: listing.map(|entries| {
                        let listing = entries
                            .into_iter()
                            .filter_map(|entry| {
                                Some(WorktreeInfo {
                                    path: entry.path.filter(|path| !path.trim().is_empty())?,
                                    label: None,
                                    name: entry.name,
                                    branch: entry.branch,
                                })
                            })
                            .collect();
                        ownership::linked_worktrees(project_path, listing)
                    }),
                });
            }
        }
        Ok(reads)
    }

    /// The running state of every session the host tracks, from its own
    /// event stream. The read starts no OpenCode location. Sessions it does
    /// not list are idle; unknown states are left out.
    pub async fn session_statuses(&self) -> Result<HashMap<SessionId, LiveStatus>, TransportError> {
        let snapshot: wire::StatusSnapshot = self
            .get_json(self.endpoint.url("api/sessions/status"), None)
            .await?;
        Ok(snapshot
            .sessions
            .into_iter()
            .filter_map(|(id, entry)| {
                let status = match entry.status.as_str() {
                    "busy" => LiveStatus::Busy,
                    "retry" => LiveStatus::Retry,
                    "idle" => LiveStatus::Idle,
                    _ => return None,
                };
                Some((SessionId::new(id), status))
            })
            .collect())
    }

    /// The server's home directory and managed Chats roots.
    pub async fn chat_roots(&self) -> Result<Option<ChatRoots>, TransportError> {
        let home: wire::HomeInfo = self
            .get_json(self.endpoint.url("api/fs/home"), None)
            .await?;
        Ok(ChatRoots::from_home_info(
            home.home.as_deref(),
            home.chats_root.as_deref(),
            home.canonical_chats_root.as_deref(),
            home.canonical_legacy_chats_root.as_deref(),
        ))
    }

    pub(crate) fn authorized(&self, method: Method, url: Url) -> RequestBuilder {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(value) = self.credential.authorization() {
            let mut value =
                HeaderValue::from_str(&value).expect("client tokens are printable ASCII");
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        self.http.request(method, url).headers(headers)
    }

    /// A GET with the credential, scoped to `directory` when one is named.
    async fn get_json<T: DeserializeOwned>(
        &self,
        url: Url,
        directory: Option<&str>,
    ) -> Result<T, TransportError> {
        let mut request = self.authorized(Method::GET, url);
        if let Some(directory) = directory {
            request = request.header("x-opencode-directory", encode_directory(directory));
        }
        let response = request.send().await.map_err(TransportError::from_request)?;
        read_json(response).await
    }
}

/// Projects a newest-first message page into server order.
///
/// A page shorter than `limit` is the oldest one, whatever cursor the server
/// still names, as `getSessionMessages` in `lib/opencode/client.ts` decides.
pub(crate) fn message_page(
    page: wire::Page<wire::MessageEnvelope>,
    limit: usize,
) -> Result<MessagePage, TransportError> {
    let mut messages = Vec::with_capacity(page.data.len());
    for envelope in page.data {
        if let Some(message) = projection::message(envelope)? {
            messages.push(message);
        }
    }
    let oldest = messages.len() < limit;
    // The server lists newest first; transcripts hold server order oldest first.
    messages.reverse();
    Ok(MessagePage {
        messages,
        older: if oldest { None } else { page.cursor.next },
    })
}

/// Percent-encodes a directory as `encodeURIComponent` does.
pub fn encode_directory(directory: &str) -> String {
    let mut encoded = String::with_capacity(directory.len());
    for byte in directory.bytes() {
        let unreserved = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if unreserved {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Reads a JSON body after checking status and content type.
async fn read_json<T: DeserializeOwned>(response: Response) -> Result<T, TransportError> {
    let status = response.status();
    if status.is_redirection() {
        return Err(TransportError::Redirected);
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(TransportError::AuthenticationRequired);
    }
    if !status.is_success() {
        return Err(TransportError::Http {
            status: status.as_u16(),
        });
    }
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !is_json {
        return Err(TransportError::Malformed(
            "the response is not JSON".to_owned(),
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(TransportError::from_request)?;
    serde_json::from_slice(&bytes).map_err(|error| TransportError::Malformed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_directories_like_encode_uri_component() {
        assert_eq!(
            encode_directory("/home/me/my project"),
            "%2Fhome%2Fme%2Fmy%20project"
        );
        assert_eq!(encode_directory("C:\\work\\app"), "C%3A%5Cwork%5Capp");
        assert_eq!(encode_directory("/tmp/ünï"), "%2Ftmp%2F%C3%BCn%C3%AF");
        assert_eq!(encode_directory("a-b_c.d!e~f*g'h(i)"), "a-b_c.d!e~f*g'h(i)");
    }
}
