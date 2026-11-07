//! An HTML file shown as a page in the Files view's preview
//! (`useHtmlPreviewUrl.ts`).
//!
//! The page loads from `/api/fs/preview/<grant>/<path>`, a grant the server
//! mints for this page with the client's credential; the grant in the path
//! is the page's only credential, so relative URLs keep it and nothing in
//! the page's address is a session token. The page sits in a shim's iframe
//! sandboxed without `allow-same-origin`, and the view's policy keeps every
//! frame inside the grant.

use gpui_kit::{Context, Entity, IntoElement, ParentElement, Render, Styled, Task, Window, div};
use openchamber_native_transport::web_content::FrameGrant;

use crate::files_view::{PanelScope, empty_state};
use crate::locale;
use crate::theme::Palette;
use crate::web_policy::{self, ServerBase, ViewPolicy};
use crate::web_shim::{self, FrameSource};
use crate::webview_host::{HostElement, HostedWebView, Load, ViewSpec, new_nonce};
use crate::webview_platform::Profile;

/// Whether `path` names an HTML page the preview shows as one.
pub(crate) fn is_html(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".html") || lower.ends_with(".htm")
}

enum State {
    Granting {
        _task: Task<()>,
    },
    Granted {
        grant: FrameGrant,
        base: String,
        url: String,
    },
    Shown(Entity<HostedWebView>),
    Failed,
}

pub(crate) struct HtmlPreviewView {
    path: String,
    /// The file revision the page was granted for; a save grants again.
    revision: u64,
    state: State,
}

impl HtmlPreviewView {
    pub(crate) fn new(
        scope: &mut PanelScope,
        path: String,
        revision: u64,
        outside: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = match scope.client(cx) {
            Some(client) => {
                let directory = scope.directory.clone();
                let page = path.clone();
                let request = scope.run(async move {
                    let grant = client
                        .html_preview_grant(&directory, &page, outside)
                        .await
                        .ok()?;
                    let url = client.html_preview_url(&grant, &page);
                    Some((grant, client.endpoint().as_str().to_owned(), url))
                });
                let task = cx.spawn(async move |this, cx| {
                    let granted = request.await.flatten();
                    let _ = this.update(cx, |this, cx| {
                        this.state = match granted {
                            Some((grant, base, url)) => State::Granted { grant, base, url },
                            None => State::Failed,
                        };
                        cx.notify();
                    });
                });
                State::Granting { _task: task }
            }
            None => State::Failed,
        };
        Self {
            path,
            revision,
            state,
        }
    }

    /// Whether this view still shows `path` at `revision`.
    pub(crate) fn shows(&self, path: &str, revision: u64) -> bool {
        self.path == path && self.revision == revision
    }

    fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let State::Granted { grant, base, url } = &self.state else {
            return;
        };
        let nonce = new_nonce();
        let spec = ViewSpec {
            policy: ViewPolicy::Preview {
                shim: web_policy::shim_url(&nonce),
                server: ServerBase::new(base),
                grant: grant.grant.clone(),
            },
            profile: Profile::Content,
            load: Load::Shim {
                nonce,
                document: web_shim::shim_document(
                    &FrameSource::Address(url.clone()),
                    web_shim::PAGE_SANDBOX,
                    &crate::files_view::file_name(&self.path),
                ),
            },
            forward_wheel: false,
            developer_tools: false,
        };
        self.state = match HostedWebView::build(spec, window, cx) {
            Ok(host) => State::Shown(host),
            Err(error) => {
                tracing::warn!(%error, "an HTML preview cannot be shown");
                State::Failed
            }
        };
    }
}

impl Render for HtmlPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.show(window, cx);
        let palette = cx.global::<Palette>().clone();
        match &self.state {
            State::Shown(host) => div()
                .size_full()
                .child(HostElement::new(host.clone()))
                .into_any_element(),
            State::Failed => empty_state(
                locale::strings(cx).get("contextPanel.browser.loadFailed"),
                &palette,
                cx,
            ),
            State::Granting { .. } | State::Granted { .. } => div().size_full().into_any_element(),
        }
    }
}
