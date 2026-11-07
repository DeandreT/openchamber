//! Routes behind the desktop's embedded web content: the grants that let a
//! sandboxed page load from the server without a credential, and the answer
//! to a page an agent is waiting on.
//!
//! A grant sits in the page's URL path, so the page's own relative requests
//! keep it; the client's bearer credential is only ever sent here, never put
//! in a URL a page can read (`useHtmlPreviewUrl.ts`, `frame-url.ts`).

use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{ServerClient, encode_directory, read_json};
use crate::error::TransportError;

/// A grant for one HTML preview page or one extension's package files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameGrant {
    pub grant: String,
}

#[derive(Deserialize)]
struct GrantBody {
    grant: Option<String>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
}

/// An installed extension with a rail panel the user may open: enabled, its
/// requested capabilities granted, and a panel entry (`isGuestActive`,
/// `hasGuestPage`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestPanel {
    pub id: String,
    pub name: String,
    /// A Remixicon name or a package SVG path.
    pub icon: String,
    pub entry: String,
}

#[derive(Deserialize)]
struct CatalogBody {
    guests: Vec<Value>,
}

/// One catalog row as a rail panel. Rows are read one by one, so a row
/// this build cannot read never hides the others.
fn guest_panel(row: &Value) -> Option<GuestPanel> {
    let text = |key: &str| {
        row.get(key)?
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let id = text("id")?;
    let valid_id = id.starts_with(|first: char| first.is_ascii_lowercase())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !valid_id || row.get("enabled").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let capabilities = row.get("capabilities")?;
    let list = |key: &str| -> Option<Vec<&str>> {
        capabilities
            .get(key)?
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect()
    };
    let (requested, granted) = (list("requested")?, list("granted")?);
    if !requested
        .iter()
        .all(|capability| granted.contains(capability))
    {
        return None;
    }
    Some(GuestPanel {
        id,
        name: text("name")?,
        icon: text("icon")?,
        entry: text("entry")?,
    })
}

/// Why an answer to a page did not reach the agent.
#[derive(Debug, thiserror::Error)]
pub enum AnswerError {
    /// The server refused it with its own message: the page no longer
    /// waits, or the answer is malformed.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Transport(#[from] TransportError),
}

fn grant_of(body: GrantBody) -> Result<FrameGrant, TransportError> {
    body.grant
        .filter(|grant| !grant.is_empty())
        .map(|grant| FrameGrant { grant })
        .ok_or_else(|| TransportError::Malformed("the grant response has no grant".to_owned()))
}

/// Percent-encodes one path segment as `encodeURIComponent` does.
pub fn encode_segment(segment: &str) -> String {
    encode_directory(segment)
}

impl ServerClient {
    /// `POST /api/fs/preview`: a grant for the HTML page at `path`, read
    /// from `directory` like the editor reads it.
    pub async fn html_preview_grant(
        &self,
        directory: &str,
        path: &str,
        outside_workspace: bool,
    ) -> Result<FrameGrant, TransportError> {
        let mut url = self.endpoint().url("api/fs/preview");
        {
            let mut query = url.query_pairs_mut();
            if !directory.is_empty() {
                query.append_pair("directory", directory);
            }
            if outside_workspace {
                query.append_pair("allowOutsideWorkspace", "true");
            }
        }
        let response = self
            .authorized(Method::POST, url)
            .json(&json!({ "path": path }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        grant_of(read_json(response).await?)
    }

    /// The address of `page_path` under a preview grant. The page loads it
    /// with no credential of its own.
    pub fn html_preview_url(&self, grant: &FrameGrant, page_path: &str) -> String {
        let encoded: Vec<String> = page_path.split('/').map(encode_directory).collect();
        let encoded = encoded.join("/");
        let absolute = if encoded.starts_with('/') {
            encoded
        } else {
            format!("/{encoded}")
        };
        self.endpoint()
            .url(&format!(
                "api/fs/preview/{}{absolute}",
                encode_directory(&grant.grant)
            ))
            .to_string()
    }

    /// `GET /api/guests`: the extensions with a rail panel to open.
    pub async fn guest_panels(&self) -> Result<Vec<GuestPanel>, TransportError> {
        let body: CatalogBody = self
            .get_json(self.endpoint().url("api/guests"), None)
            .await?;
        Ok(body.guests.iter().filter_map(guest_panel).collect())
    }

    /// `POST /api/guests/:id/frame-grant`: the grant an extension frame
    /// loads its package files with. `None` from a server that predates
    /// grants, which the native host does not support.
    pub async fn guest_frame_grant(
        &self,
        guest_id: &str,
    ) -> Result<Option<FrameGrant>, TransportError> {
        let url = self.endpoint().url(&format!(
            "api/guests/{}/frame-grant",
            encode_directory(guest_id)
        ));
        let response = self
            .authorized(Method::POST, url)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        if matches!(
            response.status(),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ) {
            return Ok(None);
        }
        grant_of(read_json(response).await?).map(Some)
    }

    /// The address of an extension's package file under its frame grant.
    pub fn guest_file_url(&self, grant: &FrameGrant, entry: &str) -> String {
        let entry: Vec<String> = entry.split('/').map(encode_directory).collect();
        self.endpoint()
            .url(&format!(
                "api/guest-files/{}/{}",
                encode_directory(&grant.grant),
                entry.join("/")
            ))
            .to_string()
    }

    /// `POST /api/openchamber/render/answer`: the user's answer to a page an
    /// agent waits on. A refusal carries the server's message, which the
    /// page receives as the reason its submit failed.
    pub async fn answer_rendered_page(
        &self,
        session_id: &str,
        call_id: &str,
        summary: &str,
        data: Value,
    ) -> Result<(), AnswerError> {
        let response = self
            .authorized(
                Method::POST,
                self.endpoint().url("api/openchamber/render/answer"),
            )
            .json(&json!({
                "sessionId": session_id,
                "callId": call_id,
                "summary": summary,
                "data": data,
            }))
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(TransportError::AuthenticationRequired.into());
        }
        let message = response
            .json::<ErrorBody>()
            .await
            .ok()
            .and_then(|body| body.error)
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| format!("The answer was not delivered ({})", status.as_u16()));
        Err(AnswerError::Refused(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::Credential;
    use crate::endpoint::Endpoint;

    fn client() -> ServerClient {
        ServerClient::new(
            Endpoint::parse("https://host.example/oc/").expect("valid"),
            Credential::None,
        )
        .expect("client")
    }

    #[test]
    fn preview_urls_keep_the_grant_in_the_path() {
        let grant = FrameGrant {
            grant: "g/1".to_owned(),
        };
        assert_eq!(
            client().html_preview_url(&grant, "/home/me/site/a b.html"),
            "https://host.example/oc/api/fs/preview/g%2F1/home/me/site/a%20b.html"
        );
    }

    #[test]
    fn only_active_extensions_with_panels_reach_the_rail() {
        let row = |extra: Value| {
            let mut row = json!({
                "id": "notes", "name": "Notes", "icon": "book", "entry": "index.html",
                "capabilities": { "requested": ["files"], "granted": ["files"] }
            });
            if let (Some(row), Some(extra)) = (row.as_object_mut(), extra.as_object()) {
                for (key, value) in extra {
                    row.insert(key.clone(), value.clone());
                }
            }
            row
        };
        assert_eq!(
            guest_panel(&row(json!({}))).map(|panel| panel.entry),
            Some("index.html".to_owned())
        );
        assert!(guest_panel(&row(json!({ "enabled": false }))).is_none());
        assert!(
            guest_panel(&row(
                json!({ "capabilities": { "requested": ["files"], "granted": [] } })
            ))
            .is_none()
        );
        assert!(guest_panel(&row(json!({ "entry": null }))).is_none());
        assert!(guest_panel(&row(json!({ "id": "Bad Id" }))).is_none());
    }

    #[test]
    fn guest_file_urls_encode_each_segment() {
        let grant = FrameGrant {
            grant: "abc".to_owned(),
        };
        assert_eq!(
            client().guest_file_url(&grant, "panel/index.html"),
            "https://host.example/oc/api/guest-files/abc/panel/index.html"
        );
    }
}
