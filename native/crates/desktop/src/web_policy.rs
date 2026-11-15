//! Which addresses each kind of embedded web view may load.
//!
//! Ports the address-bar rules of `packages/ui/src/lib/browser/url.ts`, the
//! window-open and permission rules of
//! `packages/electron/browser-panel-security.mjs` and the frame rule of
//! `packages/electron/guest-frame-navigation.mjs`. Every load the desktop
//! starts passes [`ViewPolicy::allows`] before WebKit sees it: WebKit runs a
//! `javascript:` URL given to `load_uri` without asking its navigation
//! policy, so the check has to happen here.

use openchamber_native_transport::Url;

pub(crate) const BLANK_URL: &str = "about:blank";
/// The host name the browser panel reaches dev-server tunnels under.
const DEV_TUNNEL_HOSTNAME: &str = "openchamber-preview.localhost";
/// The scheme of the desktop's own documents: the frame shim every HTML
/// answer, preview and extension panel loads into. It is the origin the
/// server already allows as a frame ancestor (`buildFrameAncestorsPolicy`).
pub(crate) const SHIM_SCHEME: &str = "openchamber-ui";
pub(crate) const SHIM_HOST: &str = "app";

const LOOPBACK_HOSTNAMES: [&str; 6] = [
    "localhost",
    "127.0.0.1",
    "0.0.0.0",
    "::1",
    "[::1]",
    DEV_TUNNEL_HOSTNAME,
];

fn is_loopback_host(host: &str) -> bool {
    LOOPBACK_HOSTNAMES
        .iter()
        .any(|loopback| loopback.eq_ignore_ascii_case(host))
}

/// `localhost:5173`, `127.0.0.1:3000`: an authority with no scheme.
fn is_loopback_authority(value: &str) -> bool {
    let host = value
        .split('/')
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    let hostname = if host.starts_with('[') {
        host.find(']').map_or(host, |end| &host[..=end])
    } else {
        host.split(':').next().unwrap_or_default()
    };
    is_loopback_host(hostname)
}

/// What the user typed, as an address the browser can load.
///
/// Schemeless input defaults to `https:`, except loopback authorities: dev
/// servers speak plain HTTP. Anything that is not http(s) becomes
/// `about:blank`.
pub(crate) fn normalize_browser_url(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return BLANK_URL.to_owned();
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        let scheme = if is_loopback_authority(trimmed) {
            "http"
        } else {
            "https"
        };
        format!("{scheme}://{trimmed}")
    };
    match Url::parse(&with_scheme) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => parsed.to_string(),
        _ => BLANK_URL.to_owned(),
    }
}

/// Whether `value` points at this machine.
pub(crate) fn is_loopback_url(value: &str) -> bool {
    Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(is_loopback_host))
        .unwrap_or(false)
}

/// A tab's short label: host and port, else the raw value.
pub(crate) fn browser_url_label(value: &str) -> String {
    if value.is_empty() || value == BLANK_URL {
        return String::new();
    }
    match Url::parse(value) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_owned(),
            _ => value.to_owned(),
        },
        Err(_) => value.to_owned(),
    }
}

fn is_http(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

/// Schemes whose documents are built from what the frame already has, with
/// no network request.
fn is_local_document(url: &Url) -> bool {
    matches!(url.scheme(), "about" | "data" | "blob")
}

/// Where a page's request for a new window goes
/// (`browserPanelWindowOpenTarget`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowOpenTarget {
    /// A scripted popup, usually a sign-in: a real window that keeps
    /// `window.opener`.
    Popup,
    Tab,
    BackgroundTab,
    InPlace,
    Deny,
}

/// How the page asked for the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenRequest {
    /// `window.open` from script.
    Script,
    /// A link meant for a new tab, in front.
    ForegroundTab,
    /// A link opened with a modifier or the middle button.
    BackgroundTab,
    /// A form whose submission carries a body.
    FormPost,
}

pub(crate) fn window_open_target(url: &str, request: OpenRequest) -> WindowOpenTarget {
    if !Url::parse(url).is_ok_and(|url| is_http(&url)) {
        return WindowOpenTarget::Deny;
    }
    match request {
        OpenRequest::Script => WindowOpenTarget::Popup,
        OpenRequest::FormPost => WindowOpenTarget::InPlace,
        OpenRequest::ForegroundTab => WindowOpenTarget::Tab,
        OpenRequest::BackgroundTab => WindowOpenTarget::BackgroundTab,
    }
}

/// The prefix every route of the selected server joins onto, such as
/// `https://host/oc/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServerBase(String);

impl ServerBase {
    pub(crate) fn new(base: &str) -> Self {
        let mut base = base.to_owned();
        if !base.ends_with('/') {
            base.push('/');
        }
        Self(base)
    }

    /// The path after the server's prefix, when `url` is on this server.
    fn route<'a>(&self, url: &'a str) -> Option<&'a str> {
        url.strip_prefix(self.0.as_str())
    }
}

/// What a view shows, which decides every address it may load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ViewPolicy {
    /// A browser tab: web pages.
    Browser,
    /// A sign-in popup a browser page opened.
    Popup,
    /// A shim holding an agent's HTML page in a `srcdoc` frame.
    HtmlAnswer { shim: String },
    /// A shim holding an HTML file preview of one grant.
    Preview {
        shim: String,
        server: ServerBase,
        grant: String,
    },
    /// A shim holding one extension's panel.
    Guest {
        shim: String,
        server: ServerBase,
        grant: String,
    },
}

impl ViewPolicy {
    /// Whether the view may load `url` in any of its frames.
    pub(crate) fn allows(&self, url: &str) -> bool {
        let Ok(parsed) = Url::parse(url) else {
            return false;
        };
        match self {
            Self::Browser | Self::Popup => {
                is_http(&parsed) || (parsed.scheme() == "about") || is_local_document(&parsed)
            }
            Self::HtmlAnswer { shim } => url == shim || is_local_document(&parsed),
            Self::Preview {
                shim,
                server,
                grant,
            } => {
                url == shim
                    || is_local_document(&parsed)
                    || server
                        .route(url)
                        .and_then(|route| route.strip_prefix("api/fs/preview/"))
                        .and_then(|rest| rest.split_once('/'))
                        .is_some_and(|(segment, _)| segment == grant)
            }
            Self::Guest {
                shim,
                server,
                grant,
            } => {
                url == shim
                    || is_local_document(&parsed)
                    || server.route(url).is_some_and(|route| {
                        route
                            .strip_prefix("api/guest-files/")
                            .and_then(|rest| rest.split_once('/'))
                            .is_some_and(|(segment, _)| segment == grant)
                    })
            }
        }
    }

    /// Whether the user's typed or chosen address may start a load. Only
    /// browser tabs take addresses, and only http(s) or `about:blank`.
    pub(crate) fn allows_load(&self, url: &str) -> bool {
        matches!(self, Self::Browser | Self::Popup)
            && (url == BLANK_URL || Url::parse(url).is_ok_and(|url| is_http(&url)))
    }
}

/// The address of a shim document, unique to one view.
pub(crate) fn shim_url(nonce: &str) -> String {
    format!("{SHIM_SCHEME}://{SHIM_HOST}/native-frame/{nonce}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_addresses_default_like_the_web_panel() {
        assert_eq!(
            normalize_browser_url("localhost:5173"),
            "http://localhost:5173/"
        );
        assert_eq!(
            normalize_browser_url("127.0.0.1:3000/app"),
            "http://127.0.0.1:3000/app"
        );
        assert_eq!(normalize_browser_url("[::1]:8080"), "http://[::1]:8080/");
        assert_eq!(normalize_browser_url("example.com"), "https://example.com/");
        assert_eq!(normalize_browser_url("  "), BLANK_URL);
        for refused in [
            "file:///etc/passwd",
            "javascript://alert(1)",
            "ftp://host/x",
            "openchamber-ui://app/native-frame/x",
        ] {
            assert_eq!(normalize_browser_url(refused), BLANK_URL, "{refused}");
        }
    }

    #[test]
    fn labels_show_host_and_port() {
        assert_eq!(
            browser_url_label("http://localhost:5173/x"),
            "localhost:5173"
        );
        assert_eq!(browser_url_label("https://example.com/a"), "example.com");
        assert_eq!(browser_url_label(BLANK_URL), "");
        assert!(is_loopback_url(
            "http://openchamber-preview.localhost:41000/"
        ));
        assert!(!is_loopback_url("https://example.com/"));
    }

    #[test]
    fn browser_tabs_load_only_web_addresses() {
        let policy = ViewPolicy::Browser;
        for allowed in [
            "https://example.com/",
            "http://localhost:3000/",
            BLANK_URL,
            "about:srcdoc",
            "data:text/html,hi",
            "blob:https://example.com/1",
        ] {
            assert!(policy.allows(allowed), "{allowed}");
        }
        for refused in [
            "file:///etc/passwd",
            "javascript:document.title='x'",
            "openchamber-ui://app/native-frame/n",
            "ftp://host/",
            "mailto:me@example.com",
            "custom-scheme://x",
        ] {
            assert!(!policy.allows(refused), "{refused}");
        }
        assert!(policy.allows_load("https://example.com/"));
        assert!(!policy.allows_load("data:text/html,hi"));
        assert!(!policy.allows_load("javascript:alert(1)"));
    }

    #[test]
    fn answer_frames_never_reach_the_network() {
        let shim = shim_url("n1");
        let policy = ViewPolicy::HtmlAnswer { shim: shim.clone() };
        assert!(policy.allows(&shim));
        assert!(policy.allows("about:srcdoc"));
        assert!(!policy.allows("https://example.com/?leak=1"));
        assert!(!policy.allows(&shim_url("other")));
        assert!(!policy.allows_load(&shim));
    }

    #[test]
    fn previews_stay_inside_their_grant() {
        let shim = shim_url("n2");
        let policy = ViewPolicy::Preview {
            shim,
            server: ServerBase::new("http://127.0.0.1:4000/oc"),
            grant: "g1".to_owned(),
        };
        assert!(policy.allows("http://127.0.0.1:4000/oc/api/fs/preview/g1/home/me/a.html"));
        assert!(!policy.allows("http://127.0.0.1:4000/oc/api/fs/preview/g2/home/me/a.html"));
        assert!(!policy.allows("http://127.0.0.1:4000/oc/api/fs/raw?path=/etc/passwd"));
        assert!(!policy.allows("http://127.0.0.1:4000/api/fs/preview/g1/x"));
        assert!(!policy.allows("https://example.com/api/fs/preview/g1/x"));
    }

    #[test]
    fn extension_frames_stay_inside_their_package() {
        let shim = shim_url("n3");
        let policy = ViewPolicy::Guest {
            shim,
            server: ServerBase::new("http://127.0.0.1:4000/"),
            grant: "abc".to_owned(),
        };
        assert!(policy.allows("http://127.0.0.1:4000/api/guest-files/abc/index.html"));
        assert!(!policy.allows("http://127.0.0.1:4000/api/guest-files/other/index.html"));
        assert!(!policy.allows("http://127.0.0.1:4000/api/session"));
        assert!(!policy.allows("https://evil.example/?data=1"));
    }

    #[test]
    fn window_requests_follow_the_desktop_shell() {
        assert_eq!(
            window_open_target("https://accounts.example/", OpenRequest::Script),
            WindowOpenTarget::Popup
        );
        assert_eq!(
            window_open_target("https://example.com/", OpenRequest::ForegroundTab),
            WindowOpenTarget::Tab
        );
        assert_eq!(
            window_open_target("https://example.com/", OpenRequest::BackgroundTab),
            WindowOpenTarget::BackgroundTab
        );
        assert_eq!(
            window_open_target("https://example.com/", OpenRequest::FormPost),
            WindowOpenTarget::InPlace
        );
        assert_eq!(
            window_open_target("file:///etc/passwd", OpenRequest::Script),
            WindowOpenTarget::Deny
        );
    }
}
