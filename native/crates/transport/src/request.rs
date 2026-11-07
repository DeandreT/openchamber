//! One credential-failure signal for every authenticated HTTP operation.

use reqwest::{Response, StatusCode};
use serde::Serialize;
use tokio::sync::watch;

/// A provider route's whole answer.
pub(crate) struct ProviderAnswer {
    pub status: StatusCode,
    pub json: bool,
    pub bytes: Vec<u8>,
    /// A 401 of the server's own sign-in, already reported.
    pub server_lock: bool,
}

/// A 401 body that explains a provider's refusal: JSON with an `error` and
/// without the server's `locked` mark.
fn provider_refusal(json: bool, bytes: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct Body {
        error: Option<String>,
        #[serde(default)]
        locked: bool,
    }
    json && serde_json::from_slice::<Body>(bytes)
        .is_ok_and(|body| body.error.is_some() && !body.locked)
}

/// The private request builder preserves reqwest's serialization and reports
/// definitive HTTP 401 responses before feature-specific parsers consume them.
pub(crate) struct RequestBuilder {
    inner: reqwest::RequestBuilder,
    authentication_failures: watch::Sender<u64>,
}

impl RequestBuilder {
    pub(crate) fn new(
        inner: reqwest::RequestBuilder,
        authentication_failures: watch::Sender<u64>,
    ) -> Self {
        Self {
            inner,
            authentication_failures,
        }
    }

    pub(crate) fn header(mut self, key: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.inner = self.inner.header(key.as_ref(), value.as_ref());
        self
    }

    pub(crate) fn json<T: Serialize + ?Sized>(mut self, body: &T) -> Self {
        self.inner = self.inner.json(body);
        self
    }

    pub(crate) fn body(mut self, body: Vec<u8>) -> Self {
        self.inner = self.inner.body(body);
        self
    }

    /// Overrides the ordinary request deadline for a bounded streaming read.
    pub(crate) fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.inner = self.inner.timeout(timeout);
        self
    }

    pub(crate) async fn send(self) -> reqwest::Result<Response> {
        let response = self.inner.send().await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.authentication_failures
                .send_modify(|revision| *revision = revision.saturating_add(1));
        }
        Ok(response)
    }

    /// Sends a request to a route that answers 401 for a provider account
    /// the server could not use (`SOURCE_CONTROL_ACCOUNT_UNAVAILABLE`, a
    /// refused provider token). Only the server's own sign-in refusal, which
    /// carries `locked: true` or no JSON reason, reports a credential failure;
    /// a provider's 401 is that route's answer.
    pub(crate) async fn send_for_provider(self) -> reqwest::Result<ProviderAnswer> {
        let response = self.inner.send().await?;
        let status = response.status();
        let json = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"));
        let bytes = response.bytes().await?.to_vec();
        let server_lock = status == StatusCode::UNAUTHORIZED && !provider_refusal(json, &bytes);
        if server_lock {
            self.authentication_failures
                .send_modify(|revision| *revision = revision.saturating_add(1));
        }
        Ok(ProviderAnswer {
            status,
            json,
            bytes,
            server_lock,
        })
    }

    #[cfg(test)]
    pub(crate) fn build(self) -> reqwest::Result<reqwest::Request> {
        self.inner.build()
    }
}
