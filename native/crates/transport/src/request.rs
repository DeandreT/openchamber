//! One credential-failure signal for every authenticated HTTP operation.

use reqwest::{Response, StatusCode};
use serde::Serialize;
use tokio::sync::watch;

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

    pub(crate) async fn send(self) -> reqwest::Result<Response> {
        let response = self.inner.send().await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.authentication_failures
                .send_modify(|revision| *revision = revision.saturating_add(1));
        }
        Ok(response)
    }

    #[cfg(test)]
    pub(crate) fn build(self) -> reqwest::Result<reqwest::Request> {
        self.inner.build()
    }
}
