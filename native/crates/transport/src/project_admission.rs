//! Explicit folder trust and session-directory registration on the captured server.
//!
//! Registration answers one confirmed project. It grants no replacement authority
//! over the project list and no authority over worktree topology.

use std::fmt;

use openchamber_native_core::ids::{ProjectId, SessionId};
use openchamber_native_core::ownership::ConfiguredProject;
use openchamber_native_core::path;
use reqwest::{Method, Response, StatusCode};
use serde::{Deserialize, Serialize};

use crate::client::ServerClient;
use crate::error::TransportError;

const TRUST_ROUTE: &str = "api/opencode/trust";
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;

/// The kind of executable configuration the server assessed.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TrustKind {
    Plugin,
    Package,
    Mcp,
    Lsp,
    Formatter,
    Shell,
    Config,
}

/// Display-only configuration content. It never grants execution authority.
#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct TrustItem {
    pub kind: TrustKind,
    pub source: String,
    pub name: String,
    pub detail: String,
}

impl fmt::Debug for TrustItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustItem")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// A server assessment, including the exact fingerprint shown for approval.
#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct TrustAssessment {
    pub directory: String,
    pub trusted: bool,
    pub fingerprint: Option<String>,
    pub items: Vec<TrustItem>,
}

impl fmt::Debug for TrustAssessment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustAssessment")
            .field("trusted", &self.trusted)
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}

/// Saving an answer may discover configuration different from what was shown.
#[derive(Debug)]
pub enum TrustAnswer {
    Saved,
    Changed(TrustAssessment),
}

/// Registration may refuse a stale session or a newly changed configuration.
#[derive(Debug)]
pub enum ProjectAdmission {
    Confirmed(ConfiguredProject),
    SessionMoved,
    TrustRequired,
}

#[derive(Serialize)]
struct TrustBody<'a> {
    directory: &'a str,
    fingerprint: &'a Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AdmissionBody<'a> {
    session_id: &'a str,
    directory: &'a str,
}

#[derive(Deserialize)]
struct ProjectReply {
    project: Project,
}

#[derive(Deserialize)]
struct Project {
    id: String,
    path: String,
}

impl ServerClient {
    /// Reads trust without starting an OpenCode location or changing settings.
    pub async fn folder_trust(&self, directory: &str) -> Result<TrustAssessment, TransportError> {
        let mut url = self.endpoint().url(TRUST_ROUTE);
        url.query_pairs_mut().append_pair("directory", directory);
        let response = self
            .authorized(Method::GET, url)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let assessment: TrustAssessment = read(response).await?;
        validate_assessment(assessment)
    }

    /// Records only an explicitly approved fingerprint. There is no automatic approval.
    pub async fn trust_folder(
        &self,
        shown: &TrustAssessment,
    ) -> Result<TrustAnswer, TransportError> {
        let response = self
            .authorized(Method::POST, self.endpoint().url(TRUST_ROUTE))
            .json(&TrustBody {
                directory: &shown.directory,
                fingerprint: &shown.fingerprint,
            })
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let changed = response.status() == StatusCode::CONFLICT;
        let assessment = if changed {
            decode(response).await?
        } else {
            read(response).await?
        };
        let assessment = validate_assessment(assessment)?;
        if changed {
            Ok(TrustAnswer::Changed(assessment))
        } else if assessment.trusted
            && assessment.directory == shown.directory
            && assessment.fingerprint == shown.fingerprint
        {
            Ok(TrustAnswer::Saved)
        } else {
            Err(malformed())
        }
    }

    /// Appends the current session destination through the serialized server route.
    pub async fn admit_session_directory(
        &self,
        id: &SessionId,
        directory: &str,
    ) -> Result<ProjectAdmission, TransportError> {
        let response = self
            .authorized(
                Method::POST,
                self.endpoint().url("api/projects/session-directory"),
            )
            .json(&AdmissionBody {
                session_id: id.as_str(),
                directory,
            })
            .send()
            .await
            .map_err(TransportError::from_request)?;
        if response.status() == StatusCode::CONFLICT {
            return Ok(ProjectAdmission::SessionMoved);
        }
        if response.status() == StatusCode::FORBIDDEN {
            #[derive(Deserialize)]
            struct Refusal {
                code: String,
                directory: String,
            }
            let refusal: Refusal = decode(response).await?;
            if refusal.code == "opencode_trust_required"
                && path::normalize(&refusal.directory) == path::normalize(directory)
            {
                return Ok(ProjectAdmission::TrustRequired);
            }
            return Err(TransportError::Http { status: 403 });
        }
        let reply: ProjectReply = read(response).await?;
        let project = reply.project;
        if project.id.trim().is_empty() || path::normalize(&project.path).is_none() {
            return Err(malformed());
        }
        Ok(ProjectAdmission::Confirmed(ConfiguredProject {
            id: ProjectId::new(project.id),
            path: project.path,
        }))
    }
}

fn validate_assessment(assessment: TrustAssessment) -> Result<TrustAssessment, TransportError> {
    if path::normalize(&assessment.directory).is_none()
        || (!assessment.trusted
            && assessment
                .fingerprint
                .as_ref()
                .is_none_or(|value| value.is_empty()))
    {
        return Err(malformed());
    }
    Ok(assessment)
}

async fn read<T: serde::de::DeserializeOwned>(response: Response) -> Result<T, TransportError> {
    match response.status() {
        StatusCode::UNAUTHORIZED => return Err(TransportError::AuthenticationRequired),
        status if status.is_redirection() => return Err(TransportError::Redirected),
        status if !status.is_success() => {
            return Err(TransportError::Http {
                status: status.as_u16(),
            });
        }
        _ => {}
    }
    decode(response).await
}

async fn decode<T: serde::de::DeserializeOwned>(
    mut response: Response,
) -> Result<T, TransportError> {
    if !response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
    {
        return Err(malformed());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(TransportError::from_request)?
    {
        if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err(malformed());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| malformed())
}

fn malformed() -> TransportError {
    TransportError::Malformed("invalid folder admission response".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn scripted(
        responses: Vec<(u16, serde_json::Value)>,
    ) -> (
        ServerClient,
        tokio::task::JoinHandle<Vec<(String, serde_json::Value)>>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let mut recorded = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.expect("request");
                let mut bytes = Vec::new();
                let end = loop {
                    let mut buffer = [0_u8; 1024];
                    let count = socket.read(&mut buffer).await.expect("HTTP request");
                    assert!(count > 0, "request must have headers");
                    bytes.extend_from_slice(&buffer[..count]);
                    assert!(bytes.len() < 16 * 1024, "bounded synthetic request");
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8(bytes[..end].to_vec()).expect("ASCII headers");
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("authorization: bearer oc_client_synthetic")
                );
                assert!(
                    !headers
                        .to_ascii_lowercase()
                        .contains("x-opencode-directory")
                );
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("content length"))
                    })
                    .unwrap_or_default();
                while bytes.len() < end + length {
                    let mut buffer = [0_u8; 1024];
                    let count = socket.read(&mut buffer).await.expect("request body");
                    assert!(count > 0, "complete request body");
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let request_body = if length == 0 {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&bytes[end..end + length]).expect("JSON request")
                };
                recorded.push((
                    headers.lines().next().expect("request line").to_owned(),
                    request_body,
                ));
                let body = body.to_string();
                let reply = format!(
                    "HTTP/1.1 {status} Scripted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.expect("response");
            }
            recorded
        });
        let endpoint = crate::endpoint::Endpoint::parse(&format!("http://{address}/prefix/"))
            .expect("test endpoint");
        let client = ServerClient::new(
            endpoint,
            crate::credential::Credential::ClientToken("oc_client_synthetic".into()),
        )
        .expect("test client");
        (client, server)
    }

    #[tokio::test]
    async fn trust_changes_require_another_explicit_answer_and_registration_is_a_partial_append() {
        use serde_json::json;
        let assessment = |fingerprint: &str, trusted| {
            json!({"directory":"/outside", "trusted":trusted, "fingerprint":fingerprint,
            "items":[{"kind":"mcp", "source":"opencode.json", "name":"local", "detail":"synthetic-command"}]})
        };
        let (client, server) = scripted(vec![
            (200, assessment("first", false)),
            (409, assessment("second", false)),
            (200, assessment("second", true)),
            (200, json!({"project":{"id":"outside", "path":"/outside"}})),
        ])
        .await;
        let shown = client.folder_trust("/outside").await.expect("assessment");
        assert!(!shown.trusted);
        let TrustAnswer::Changed(changed) = client
            .trust_folder(&shown)
            .await
            .expect("changed assessment")
        else {
            panic!("changed configuration requires another answer");
        };
        assert_eq!(changed.fingerprint.as_deref(), Some("second"));
        assert!(matches!(
            client
                .trust_folder(&changed)
                .await
                .expect("explicit new answer"),
            TrustAnswer::Saved
        ));
        let ProjectAdmission::Confirmed(project) = client
            .admit_session_directory(&SessionId::from("ses_one"), "/outside")
            .await
            .expect("registration")
        else {
            panic!("confirmed append");
        };
        assert_eq!(project.path, "/outside");
        let requests = server.await.expect("server");
        assert_eq!(
            requests.len(),
            4,
            "assessment and changed fingerprints never trigger automatic approval"
        );
        assert!(
            requests[0]
                .0
                .starts_with("GET /prefix/api/opencode/trust?directory=%2Foutside ")
        );
        assert_eq!(
            requests[1].1,
            json!({"directory":"/outside", "fingerprint":"first"})
        );
        assert_eq!(
            requests[2].1,
            json!({"directory":"/outside", "fingerprint":"second"})
        );
        assert!(
            requests[3]
                .0
                .starts_with("POST /prefix/api/projects/session-directory ")
        );
        assert_eq!(
            requests[3].1,
            json!({"sessionId":"ses_one", "directory":"/outside"})
        );
    }

    #[tokio::test]
    async fn changed_location_and_trust_refusal_never_become_a_confirmed_project() {
        use serde_json::json;
        let (client, server) = scripted(vec![
            (409, json!({"error":"The session directory changed"})),
            (
                403,
                json!({"code":"opencode_trust_required", "directory":"/outside"}),
            ),
            (503, json!({"error":"unavailable"})),
        ])
        .await;
        let id = SessionId::from("ses_one");
        assert!(matches!(
            client
                .admit_session_directory(&id, "/outside")
                .await
                .expect("location outcome"),
            ProjectAdmission::SessionMoved
        ));
        assert!(matches!(
            client
                .admit_session_directory(&id, "/outside")
                .await
                .expect("trust outcome"),
            ProjectAdmission::TrustRequired
        ));
        assert!(
            client
                .admit_session_directory(&id, "/outside")
                .await
                .is_err()
        );
        assert_eq!(server.await.expect("server").len(), 3);
    }

    #[test]
    fn executable_configuration_needs_a_real_fingerprint() {
        let assessment = TrustAssessment {
            directory: "/outside".into(),
            trusted: false,
            fingerprint: None,
            items: Vec::new(),
        };
        assert!(validate_assessment(assessment).is_err());
        let assessment = TrustAssessment {
            directory: "/outside".into(),
            trusted: true,
            fingerprint: None,
            items: Vec::new(),
        };
        assert!(validate_assessment(assessment).is_ok());
    }

    #[test]
    fn registration_body_contains_only_the_session_and_destination() {
        let body = AdmissionBody {
            session_id: "ses_one",
            directory: "/outside",
        };
        assert_eq!(
            serde_json::to_value(body).expect("serializable body"),
            serde_json::json!({ "sessionId": "ses_one", "directory": "/outside" })
        );
    }
}
