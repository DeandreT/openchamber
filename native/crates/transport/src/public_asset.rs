//! Public assets the web client loads from third-party hosts, such as the
//! provider logos it takes from models.dev.
//!
//! The client is separate from [`crate::client::ServerClient`]: it carries no
//! credential, cookie or OpenChamber header, accepts only HTTPS, follows only
//! HTTPS redirects and reads a bounded body.

use std::time::Duration;

use reqwest::redirect;

use crate::error::TransportError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: usize = 5;
/// Logos are a few kilobytes; anything larger is not one.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Fetches public assets over HTTPS.
#[derive(Clone, Debug)]
pub struct PublicAssetClient {
    http: reqwest::Client,
}

impl PublicAssetClient {
    pub fn new() -> Result<Self, TransportError> {
        let policy = redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS || attempt.url().scheme() != "https" {
                attempt.stop()
            } else {
                attempt.follow()
            }
        });
        let http = reqwest::Client::builder()
            .redirect(policy)
            .https_only(true)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(TransportError::from_request)?;
        Ok(Self { http })
    }

    /// The body of `url`, which must be HTTPS. A status other than success
    /// is [`TransportError::Http`]; a body over the size limit is
    /// [`TransportError::Malformed`].
    pub async fn fetch(&self, url: &str) -> Result<Vec<u8>, TransportError> {
        let parsed = reqwest::Url::parse(url)
            .ok()
            .filter(|parsed| parsed.scheme() == "https")
            .ok_or_else(|| TransportError::InvalidEndpoint("not an HTTPS URL".to_owned()))?;
        let mut response = self
            .http
            .get(parsed)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        let status = response.status();
        if status.is_redirection() {
            return Err(TransportError::Redirected);
        }
        if !status.is_success() {
            return Err(TransportError::Http {
                status: status.as_u16(),
            });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(TransportError::from_request)?
        {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(TransportError::Malformed(
                    "the asset exceeds the size limit".to_owned(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_https_urls_are_fetched() {
        let client = PublicAssetClient::new().expect("the client builds without a runtime");
        for url in [
            "http://models.dev/logos/perf.svg",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(
                matches!(
                    client.fetch(url).await,
                    Err(TransportError::InvalidEndpoint(_))
                ),
                "{url} is refused before any request"
            );
        }
    }
}
