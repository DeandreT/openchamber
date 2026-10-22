//! The user-selected server endpoint and its path prefix.

use std::net::IpAddr;

use reqwest::Url;

use crate::error::TransportError;

/// An OpenChamber base URL, possibly behind a reverse-proxy path prefix.
///
/// Every route joins onto the prefix, so `https://host/oc/` reaches
/// `https://host/oc/health` and `https://host/oc/api/session`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    base: Url,
}

impl Endpoint {
    /// Parses an `http` or `https` base URL.
    ///
    /// Query strings, fragments and embedded credentials are rejected: they
    /// would either be dropped silently or leak into every request.
    pub fn parse(value: &str) -> Result<Self, TransportError> {
        let mut base = Url::parse(value.trim())
            .map_err(|error| TransportError::InvalidEndpoint(error.to_string()))?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(TransportError::InvalidEndpoint(format!(
                "unsupported scheme {}",
                base.scheme()
            )));
        }
        if base.host_str().is_none() {
            return Err(TransportError::InvalidEndpoint(
                "a host is required".to_owned(),
            ));
        }
        if !base.username().is_empty() || base.password().is_some() {
            return Err(TransportError::InvalidEndpoint(
                "credentials do not belong in the URL".to_owned(),
            ));
        }
        if base.query().is_some() || base.fragment().is_some() {
            return Err(TransportError::InvalidEndpoint(
                "a query or fragment is not part of a base URL".to_owned(),
            ));
        }
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        Ok(Self { base })
    }

    /// The URL of a route such as `health` or `api/session`.
    pub fn url(&self, route: &str) -> Url {
        self.base
            .join(route.trim_start_matches('/'))
            .expect("a relative route always joins onto a valid base URL")
    }

    /// The WebSocket URL of a route, `ws` or `wss` by the base scheme.
    pub fn websocket_url(&self, route: &str) -> Url {
        let mut url = self.url(route);
        let scheme = if self.base.scheme() == "https" {
            "wss"
        } else {
            "ws"
        };
        url.set_scheme(scheme)
            .expect("http and https URLs accept ws and wss");
        url
    }

    /// The host and port, for messages that name the server.
    pub fn authority(&self) -> String {
        let host = self.base.host_str().unwrap_or_default();
        match self.base.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        }
    }

    /// The origin and prefix, for display and runtime identity.
    pub fn as_str(&self) -> &str {
        self.base.as_str()
    }

    /// Whether the server runs on this machine.
    pub fn is_loopback(&self) -> bool {
        let host = self.base.host_str().unwrap_or_default();
        if host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_the_path_prefix() {
        let endpoint = Endpoint::parse("https://host.example/openchamber").expect("valid");
        assert_eq!(
            endpoint.url("health").as_str(),
            "https://host.example/openchamber/health"
        );
        assert_eq!(
            endpoint.url("/api/session").as_str(),
            "https://host.example/openchamber/api/session"
        );
        assert_eq!(
            endpoint.websocket_url("api/global/event/ws").as_str(),
            "wss://host.example/openchamber/api/global/event/ws"
        );
    }

    #[test]
    fn knows_servers_on_this_machine() {
        for local in [
            "http://127.0.0.1:4799",
            "http://localhost/",
            "http://[::1]:3000",
        ] {
            assert!(
                Endpoint::parse(local).expect("valid").is_loopback(),
                "{local}"
            );
        }
        for remote in ["https://host.example", "http://10.0.0.2:3000"] {
            assert!(
                !Endpoint::parse(remote).expect("valid").is_loopback(),
                "{remote}"
            );
        }
    }

    #[test]
    fn rejects_unsafe_or_unusable_urls() {
        for value in [
            "ftp://host",
            "http://user:secret@host",
            "http://host/?token=x",
            "http://host/#x",
            "not a url",
        ] {
            assert!(Endpoint::parse(value).is_err(), "{value} must be rejected");
        }
        let plain = Endpoint::parse("http://127.0.0.1:4799").expect("valid");
        assert_eq!(plain.authority(), "127.0.0.1:4799");
        assert_eq!(plain.url("health").as_str(), "http://127.0.0.1:4799/health");
        assert_eq!(
            plain.websocket_url("api/global/event/ws").as_str(),
            "ws://127.0.0.1:4799/api/global/event/ws"
        );
    }
}
