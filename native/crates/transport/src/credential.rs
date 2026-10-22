//! Credentials the transport sends, kept out of logs and URLs.

use std::fmt;

/// How requests authenticate.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// The server has no UI password.
    None,
    /// A trusted-device client token, sent as `Authorization: Bearer`.
    ClientToken(String),
}

impl Credential {
    /// The `Authorization` header value, when there is one.
    pub(crate) fn authorization(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::ClientToken(token) => Some(format!("Bearer {token}")),
        }
    }
}

impl fmt::Debug for Credential {
    // A token never reaches a log line, even through Debug output.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("Credential::None"),
            Self::ClientToken(_) => formatter.write_str("Credential::ClientToken(<redacted>)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_contains_the_token() {
        let credential = Credential::ClientToken("oc_client_secret".to_owned());
        assert!(!format!("{credential:?}").contains("secret"));
        assert_eq!(
            credential.authorization().as_deref(),
            Some("Bearer oc_client_secret")
        );
    }
}
