//! Typed transport failures.

use thiserror::Error;

/// Why a request or connection did not produce an authoritative answer.
///
/// No variant stands for an empty success: callers keep their last valid
/// state on every error.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TransportError {
    #[error("the endpoint URL is invalid: {0}")]
    InvalidEndpoint(String),
    #[error("the server could not be reached")]
    Unreachable(#[source] reqwest::Error),
    #[error("the server did not answer in time")]
    TimedOut,
    #[error("the server answered with a redirect, which is never followed")]
    Redirected,
    #[error("the endpoint is not an OpenChamber server")]
    NotOpenChamber,
    #[error("the server requires authentication")]
    AuthenticationRequired,
    #[error("the server rejected the credentials")]
    AuthenticationRejected,
    #[error("the server is incompatible: {0}")]
    Incompatible(String),
    #[error("the server answered HTTP {status}")]
    Http { status: u16 },
    #[error("the server's answer is malformed: {0}")]
    Malformed(String),
    #[error("the event stream failed: {0}")]
    Stream(String),
    #[error("the operation was cancelled")]
    Cancelled,
}

impl TransportError {
    /// Classifies a reqwest failure without exposing its URL.
    pub(crate) fn from_request(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::TimedOut
        } else if error.is_redirect() {
            Self::Redirected
        } else if error.is_decode() {
            Self::Malformed("the body could not be decoded".to_owned())
        } else {
            Self::Unreachable(error.without_url())
        }
    }
}
