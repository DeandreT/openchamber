//! The GTK and WebKit side of the desktop's web views (Linux, X11).
//!
//! GPUI runs no GLib main loop, so GTK is started once on the UI thread and
//! a GPUI task iterates GLib every 4 ms while any web view exists. WebKit
//! callbacks therefore run on the UI thread, outside any GPUI update; they
//! only queue, and the same task hands the queue to each view.
//!
//! Two profiles keep identities apart, as Electron's partitions do:
//!
//! - the browser profile, persistent under the app's config directory, holds
//!   what the user's own browsing stores (`persist:openchamber-browser`);
//! - the content profile, ephemeral, holds the frame shims of agent pages,
//!   previews and extension panels. Their content runs in opaque-origin
//!   sandboxes and stores nothing; the profile only serves the shim scheme.
//!
//! Neither profile shares anything with the desktop's own credentials, which
//! never enter a web view.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{App, Global, Task, WeakEntity};
use gtk::prelude::*;
use gtk::{gio, glib};
use webkit2gtk::{
    CookieManagerExt, CookiePersistentStorage, SecurityManagerExt, URISchemeRequestExt,
    URISchemeResponseExt, WebContextExt, WebsiteDataManagerExt,
};

use crate::web_policy::{SHIM_HOST, SHIM_SCHEME};
use crate::webview_host::{HostedWebView, XFocus};

/// How often GLib runs while a web view exists.
const PUMP_INTERVAL: Duration = Duration::from_millis(4);

/// Which identity a view loads under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    Browser,
    Content,
}

/// The shim documents waiting to be served, by nonce, with the view that
/// may load each. Each is served once.
type Shims = Rc<RefCell<HashMap<String, (glib::WeakRef<webkit2gtk::WebView>, String)>>>;

pub(crate) struct WebPlatform {
    focus: Rc<XFocus>,
    browser: webkit2gtk::WebContext,
    content: webkit2gtk::WebContext,
    shims: Shims,
    hosts: Vec<WeakEntity<HostedWebView>>,
    pump: Option<Task<()>>,
}

impl Global for WebPlatform {}

/// Why web content cannot be shown in this session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// GPUI runs on Wayland, where WebKit cannot be a child window.
    NotX11,
    /// GTK or the X connection could not start.
    Failed(String),
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotX11 => formatter.write_str("embedded web content needs an X11 session"),
            Self::Failed(reason) => {
                write!(formatter, "embedded web content failed to start: {reason}")
            }
        }
    }
}

/// A failed start, remembered so every view reports the same reason.
struct StartFailure(Unavailable);

impl Global for StartFailure {}

fn browser_profile_directory() -> Option<PathBuf> {
    crate::install_id::config_directory().map(|directory| directory.join("browser-panel"))
}

/// A random hexadecimal identifier from the kernel's generator.
pub(crate) fn random_id() -> String {
    let mut bytes = [0_u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .expect("the kernel's random source is readable");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl WebPlatform {
    /// Starts GTK and both profiles on first use.
    pub(crate) fn ensure(cx: &mut App) -> Result<(), Unavailable> {
        if cx.has_global::<WebPlatform>() {
            return Ok(());
        }
        if let Some(failure) = cx.try_global::<StartFailure>() {
            return Err(failure.0.clone());
        }
        match Self::start() {
            Ok(platform) => {
                cx.set_global(platform);
                Ok(())
            }
            Err(error) => {
                tracing::warn!(%error, "web views are unavailable");
                cx.set_global(StartFailure(error.clone()));
                Err(error)
            }
        }
    }

    fn start() -> Result<Self, Unavailable> {
        if std::env::var_os("DISPLAY").is_none() {
            return Err(Unavailable::NotX11);
        }
        // GPUI owns an X11 window; GTK must use the same server.
        gtk::gdk::set_allowed_backends("x11");
        gtk::init().map_err(|error| Unavailable::Failed(error.to_string()))?;
        let focus = XFocus::connect().map_err(Unavailable::Failed)?;
        let browser = Self::browser_context();
        let content = Self::content_context();
        let shims = Shims::default();
        let served = shims.clone();
        content.register_uri_scheme(SHIM_SCHEME, move |request| serve_shim(request, &served));
        Ok(Self {
            focus: Rc::new(focus),
            browser,
            content,
            shims,
            hosts: Vec::new(),
            pump: None,
        })
    }

    fn browser_context() -> webkit2gtk::WebContext {
        let manager = match browser_profile_directory() {
            Some(directory) => {
                // WebKit shares `$XDG_CACHE_HOME/<program>` between every
                // profile unless the cache directory is named too.
                let manager = webkit2gtk::WebsiteDataManager::builder()
                    .base_data_directory(directory.join("data").to_string_lossy().into_owned())
                    .base_cache_directory(directory.join("cache").to_string_lossy().into_owned())
                    .build();
                if let Some(cookies) = manager.cookie_manager() {
                    cookies.set_persistent_storage(
                        &directory.join("cookies.sqlite").to_string_lossy(),
                        CookiePersistentStorage::Sqlite,
                    );
                }
                manager
            }
            None => webkit2gtk::WebsiteDataManager::new_ephemeral(),
        };
        let context = webkit2gtk::WebContext::builder()
            .website_data_manager(&manager)
            .build();
        // Downloads have no destination flow yet; nothing is written.
        context.connect_download_started(|_, download| {
            use webkit2gtk::DownloadExt;
            download.cancel();
        });
        context
    }

    fn content_context() -> webkit2gtk::WebContext {
        let manager = webkit2gtk::WebsiteDataManager::new_ephemeral();
        let context = webkit2gtk::WebContext::builder()
            .website_data_manager(&manager)
            .build();
        if let Some(security) = context.security_manager() {
            // Only shim documents can show shim documents: a page of any
            // other scheme cannot frame or navigate to one.
            security.register_uri_scheme_as_display_isolated(SHIM_SCHEME);
        }
        context.connect_download_started(|_, download| {
            use webkit2gtk::DownloadExt;
            download.cancel();
        });
        context
    }

    pub(crate) fn context(cx: &App, profile: Profile) -> webkit2gtk::WebContext {
        let platform = cx.global::<WebPlatform>();
        match profile {
            Profile::Browser => platform.browser.clone(),
            Profile::Content => platform.content.clone(),
        }
    }

    pub(crate) fn focus(cx: &App) -> Rc<XFocus> {
        cx.global::<WebPlatform>().focus.clone()
    }

    /// Lets `view` load the shim at `nonce` once.
    pub(crate) fn offer_shim(cx: &App, nonce: &str, view: &webkit2gtk::WebView, document: String) {
        let mut shims = cx.global::<WebPlatform>().shims.borrow_mut();
        // A view dropped before its load leaves an entry nobody can claim.
        shims.retain(|_, (view, _)| view.upgrade().is_some());
        shims.insert(nonce.to_owned(), (view.downgrade(), document));
    }

    /// Keeps GLib running for `host` and hands it its queued events.
    pub(crate) fn register(host: WeakEntity<HostedWebView>, cx: &mut App) {
        let platform = cx.global_mut::<WebPlatform>();
        platform.hosts.push(host);
        if platform.pump.is_none() {
            let pump = cx.spawn(async move |cx| {
                loop {
                    cx.background_executor().timer(PUMP_INTERVAL).await;
                    let live = cx.update(|cx| {
                        while gtk::events_pending() {
                            gtk::main_iteration_do(false);
                        }
                        WebPlatform::tick(cx)
                    });
                    if !live {
                        break;
                    }
                }
            });
            cx.global_mut::<WebPlatform>().pump = Some(pump);
        }
    }

    /// Delivers each live view's events and brings its window up to date.
    /// Returns whether any view is still alive; the pump stops otherwise,
    /// after letting GLib finish the last view's teardown.
    fn tick(cx: &mut App) -> bool {
        let hosts: Vec<_> = {
            let platform = cx.global_mut::<WebPlatform>();
            platform.hosts.retain(|host| host.upgrade().is_some());
            platform.hosts.clone()
        };
        for host in &hosts {
            if let Some(host) = host.upgrade() {
                HostedWebView::tick(&host, cx);
            }
        }
        let platform = cx.global_mut::<WebPlatform>();
        if platform.hosts.is_empty() {
            platform.pump = None;
            return false;
        }
        true
    }
}

/// Answers a request for a shim document: only the view it was offered to
/// gets it, and anything else under the scheme is not found.
fn serve_shim(request: &webkit2gtk::URISchemeRequest, shims: &Shims) {
    let nonce = request
        .uri()
        .and_then(|uri| {
            uri.strip_prefix(&format!("{SHIM_SCHEME}://{SHIM_HOST}/native-frame/"))
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let owner = shims.borrow().get(&nonce).is_some_and(|(view, _)| {
        view.upgrade()
            .is_some_and(|view| request.web_view() == Some(view))
    });
    let document = owner
        .then(|| shims.borrow_mut().remove(&nonce))
        .flatten()
        .map(|(_, document)| document);
    let Some(document) = document else {
        let mut error = glib::Error::new(gio::IOErrorEnum::NotFound, "not found");
        request.finish_error(&mut error);
        return;
    };
    let bytes = glib::Bytes::from_owned(document.into_bytes());
    let length = bytes.len() as i64;
    let stream = gio::MemoryInputStream::from_bytes(&bytes);
    let response = webkit2gtk::URISchemeResponse::new(&stream, length);
    response.set_content_type("text/html; charset=utf-8");
    response.set_status(200, None);
    request.finish_with_response(&response);
}
