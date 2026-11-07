//! An extension's rail panel (`PluginPane.tsx`).
//!
//! The panel loads from `/api/guest-files/<grant>/<entry>`: the desktop mints
//! the grant with its own credential, and the grant in the path is the
//! frame's only credential, so the page never sees the client token. The
//! frame sits in a shim's iframe sandboxed with `allow-scripts` only, and the
//! view's policy keeps every frame inside that grant's package. The SDK
//! envelope is answered by [`crate::sdk_envelope`]: `ready` on `hello`, and
//! `UNSUPPORTED` for every request, since no host service is ported yet.

use gpui_kit::component::Theme;
use gpui_kit::{
    Context, Entity, Hsla, IntoElement, ParentElement, Render, Styled, Subscription, Task,
    WeakEntity, Window, div,
};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_transport::web_content::{FrameGrant, GuestPanel};

use crate::files_view::empty_state;
use crate::locale;
use crate::network::NetworkTask;
use crate::sdk_envelope::{self, GuestMessage, GuestTheme, ReadyContext};
use crate::theme::Palette;
use crate::web_policy::{self, ServerBase, ViewPolicy};
use crate::web_shim::{self, FrameSource};
use crate::webview_host::{HostElement, HostedWebView, Load, ViewSpec, WebViewEvent, new_nonce};
use crate::webview_platform::Profile;
use crate::workspace::Workspace;

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

pub(crate) struct GuestPanelView {
    panel: GuestPanel,
    runtime: RuntimeId,
    directory: Option<String>,
    owner: WeakEntity<Workspace>,
    state: State,
    _events: Option<Subscription>,
}

fn css(color: Hsla) -> String {
    let rgba = color.to_rgb();
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "rgb({} {} {} / {})",
        channel(rgba.r),
        channel(rgba.g),
        channel(rgba.b),
        (rgba.a * 1000.0).round() / 1000.0
    )
}

/// The host colour bag a panel draws with (`ready.theme.tokens`).
fn guest_theme(cx: &gpui_kit::App) -> GuestTheme {
    let palette = cx.global::<Palette>();
    let theme = Theme::global(cx);
    let radius = crate::appearance::root_font_size(cx) * 0.5625;
    let tokens = vec![
        ("background", css(palette.muted)),
        ("elevated", css(palette.elevated)),
        ("foreground", css(palette.foreground)),
        ("muted", css(palette.muted_foreground)),
        ("subtle", css(palette.subtle)),
        ("border", css(palette.border)),
        ("hover", css(palette.hover)),
        ("selection", css(palette.selection)),
        ("focus", css(palette.focus_ring)),
        ("primary", css(palette.primary)),
        ("mutedSurface", css(palette.chat_background)),
        ("elevatedForeground", css(palette.elevated_foreground)),
        ("active", css(palette.active)),
        ("selectionForeground", css(palette.selection_foreground)),
        ("primaryForeground", css(palette.primary_foreground)),
        ("primaryText", css(palette.primary)),
        ("successText", css(palette.success)),
        ("warningText", css(palette.warning)),
        ("errorText", css(palette.error)),
        ("infoText", css(palette.info)),
        ("success", css(palette.status_success)),
        ("warning", css(palette.status_warning)),
        ("error", css(palette.status_error)),
        ("info", css(palette.status_info)),
        ("font", format!("\"{}\", sans-serif", theme.font_family)),
        ("mono", format!("\"{}\", monospace", theme.mono_font_family)),
        ("radius", format!("{radius}px")),
    ];
    GuestTheme {
        dark: palette.appearance == crate::theme::Appearance::Dark,
        tokens,
    }
}

impl GuestPanelView {
    pub(crate) fn new(
        panel: GuestPanel,
        runtime: RuntimeId,
        directory: Option<String>,
        owner: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = Self::mint(&panel, &runtime, &owner, cx);
        Self {
            panel,
            runtime,
            directory,
            owner,
            state,
            _events: None,
        }
    }

    /// Asks the selected server for the frame's grant.
    fn mint(
        panel: &GuestPanel,
        runtime: &RuntimeId,
        owner: &WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> State {
        let Some(workspace) = owner.upgrade() else {
            return State::Failed;
        };
        let workspace = workspace.read(cx);
        let connection = workspace
            .connection
            .as_ref()
            .filter(|_| &workspace.runtime == runtime);
        let Some((client, network)) = connection
            .and_then(|connection| Some((connection.client.clone()?, connection.network.handle())))
        else {
            return State::Failed;
        };
        let id = panel.id.clone();
        let entry = panel.entry.clone();
        let request = NetworkTask::spawn_on(&network, async move {
            let grant = client.guest_frame_grant(&id).await.ok().flatten()?;
            let url = client.guest_file_url(&grant, &entry);
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

    fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let State::Granted { grant, base, url } = &self.state else {
            return;
        };
        let nonce = new_nonce();
        let spec = ViewSpec {
            policy: ViewPolicy::Guest {
                shim: web_policy::shim_url(&nonce),
                server: ServerBase::new(base),
                grant: grant.grant.clone(),
            },
            profile: Profile::Content,
            load: Load::Shim {
                nonce,
                document: web_shim::shim_document(
                    &FrameSource::Address(url.clone()),
                    web_shim::GUEST_SANDBOX,
                    &self.panel.name,
                ),
            },
            forward_wheel: false,
            developer_tools: false,
        };
        self.state = match HostedWebView::build(spec, window, cx) {
            Ok(host) => {
                self._events = Some(cx.subscribe_in(&host, window, |this, host, event, _, cx| {
                    if let WebViewEvent::Message(message) = event {
                        this.guest_message(host, message, cx);
                    }
                }));
                State::Shown(host)
            }
            Err(error) => {
                tracing::warn!(%error, "an extension panel cannot be shown");
                State::Failed
            }
        };
    }

    fn guest_message(
        &mut self,
        host: &Entity<HostedWebView>,
        message: &serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let reply = match sdk_envelope::read_guest_message(message) {
            Some(GuestMessage::Hello) => {
                let connected = self.owner.upgrade().is_some_and(|workspace| {
                    let workspace = workspace.read(cx);
                    workspace.runtime == self.runtime
                        && workspace
                            .connection
                            .as_ref()
                            .is_some_and(|connection| connection.client.is_some())
                });
                sdk_envelope::ready_message(&ReadyContext {
                    theme: guest_theme(cx),
                    locale: locale::strings(cx).locale().to_owned(),
                    directory: self.directory.clone(),
                    connected,
                })
            }
            Some(GuestMessage::Request { kind, id }) => {
                sdk_envelope::unsupported_result(&id, &kind)
            }
            Some(GuestMessage::Notice) | None => return,
        };
        host.read(cx).post_message(&reply);
    }
}

impl Render for GuestPanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.show(window, cx);
        let palette = cx.global::<Palette>().clone();
        let strings = locale::strings(cx);
        match &self.state {
            State::Shown(host) => div()
                .size_full()
                .child(HostElement::new(host.clone()))
                .into_any_element(),
            State::Failed => {
                empty_state(strings.get("contextPanel.browser.loadFailed"), &palette, cx)
            }
            State::Granting { .. } | State::Granted { .. } => div().size_full().into_any_element(),
        }
    }
}
