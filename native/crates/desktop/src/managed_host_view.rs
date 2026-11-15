//! What the window shows of the managed host.
//!
//! The window connects to the managed server only once it is ready, and
//! again after each recovery. While it starts or recovers, the sidebar's
//! status line says so; a failed host takes the window with the desktop's
//! `local-unavailable` recovery screen
//! (`packages/ui/src/components/onboarding/DesktopConnectionRecovery.tsx`).
//! Retry Local relaunches the app, as the desktop's Retry does; Use Remote
//! opens the host picker. Quitting and stopping happen after the window has
//! closed, so they have no presentation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui_kit::{
    Context, FocusHandle, FontWeight, InteractiveElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, Task, Window, div, px,
};
use openchamber_native_transport::endpoint::Endpoint;
use tokio::sync::watch;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome;
use crate::locale::{self, Strings};
use crate::managed_host_lifecycle::HostState;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// What the window receives of the host `main` owns.
#[derive(Debug)]
pub(crate) struct ManagedLink {
    pub(crate) state: watch::Receiver<HostState>,
    /// Set by Retry Local; `main` relaunches after the host stopped.
    pub(crate) relaunch: Arc<AtomicBool>,
}

/// The window's view of the managed host.
pub(crate) struct ManagedHostView {
    endpoint: Endpoint,
    pub(crate) state: HostState,
    relaunch: Arc<AtomicBool>,
    focus: FocusHandle,
    _watch: Task<()>,
}

impl Workspace {
    /// Follows the host and connects whenever its server is ready.
    pub(crate) fn watch_managed_host(
        &mut self,
        link: ManagedLink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(endpoint) = self
            .connection
            .as_ref()
            .map(|connection| connection.config.endpoint.clone())
        else {
            return;
        };
        let ManagedLink {
            state: mut receiver,
            relaunch,
        } = link;
        let state = *receiver.borrow_and_update();
        let watch = cx.spawn_in(window, async move |this, cx| {
            while receiver.changed().await.is_ok() {
                let state = *receiver.borrow_and_update();
                if this
                    .update_in(cx, |this, window, cx| {
                        this.on_managed_state(state, window, cx)
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        self.managed_host = Some(ManagedHostView {
            endpoint,
            state,
            relaunch,
            focus: cx.focus_handle(),
            _watch: watch,
        });
        if matches!(state, HostState::Ready { .. }) {
            self.connect(window, cx);
        }
    }

    fn on_managed_state(&mut self, state: HostState, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.managed_host.as_mut() else {
            return;
        };
        view.state = state;
        tracing::info!(?state, "managed host");
        if matches!(state, HostState::Ready { .. }) && self.shows_managed_host() {
            self.connect(window, cx);
        }
        if self.managed_failure_visible()
            && let Some(view) = &self.managed_host
        {
            view.focus.focus(window, cx);
        }
        cx.notify();
    }

    /// Whether the window shows the managed server, and not a host the user
    /// switched to.
    fn shows_managed_host(&self) -> bool {
        self.managed_host.as_ref().is_some_and(|view| {
            self.connection
                .as_ref()
                .is_some_and(|connection| connection.config.endpoint == view.endpoint)
        })
    }

    pub(crate) fn managed_failure_visible(&self) -> bool {
        self.shows_managed_host()
            && self
                .managed_host
                .as_ref()
                .is_some_and(|view| matches!(view.state, HostState::Failed(_)))
    }

    /// The sidebar's line while the managed server starts or recovers.
    pub(crate) fn managed_host_status(&self, strings: &Strings) -> Option<SharedString> {
        if !self.shows_managed_host() {
            return None;
        }
        match self.managed_host.as_ref()?.state {
            HostState::Starting => Some(strings.format(
                "desktopHostSwitcher.startup.connectingTo",
                &[("host", &strings.get("desktopHostSwitcher.instance.local"))],
            )),
            HostState::Recovering { .. } => {
                Some(strings.get("updateDialog.status.serverRestarting"))
            }
            HostState::Ready { .. }
            | HostState::Failed(_)
            | HostState::Quitting
            | HostState::Stopped => None,
        }
    }

    fn retry_managed_host(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = &self.managed_host {
            view.relaunch.store(true, Ordering::Release);
            cx.quit();
        }
    }

    pub(crate) fn render_managed_failure(
        &self,
        palette: &Palette,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let view = self
            .managed_host
            .as_ref()
            .expect("the failure view has a managed host");
        let strings = locale::strings(cx);
        let title = strings.get("onboarding.desktopRecovery.localUnavailable.title");
        let description = strings.get("onboarding.desktopRecovery.localUnavailable.description");
        let retry_label = strings.get("onboarding.desktopRecovery.localUnavailable.retry");
        let remote_label = strings.get("onboarding.desktopRecovery.common.useRemote");
        let retry = on_activate(
            gpui_kit::base::Button::new("managed-host-retry")
                .accessibility_id("managed-host-retry")
                .accessibility_label(retry_label.clone())
                .track_focus(&view.focus)
                .w_full()
                .h(px(32.0))
                .px(px(12.0))
                .rounded(px(7.0))
                .bg(palette.primary)
                .text_color(palette.primary_foreground)
                .child(retry_label),
            cx,
            |this, _, cx| this.retry_managed_host(cx),
        );
        let remote = on_activate(
            gpui_kit::base::Button::new("managed-host-remote")
                .accessibility_id("managed-host-remote")
                .accessibility_label(remote_label.clone())
                .w_full()
                .h(px(32.0))
                .px(px(12.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(palette.border)
                .text_color(palette.foreground)
                .child(remote_label),
            cx,
            |this, window, cx| this.open_host_picker(window, cx),
        );
        div()
            .id("workspace")
            .accessibility_id("workspace")
            .role(Role::Group)
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.background)
            .text_color(palette.foreground)
            .child(
                chrome::drag_region("managed-host-titlebar", cx)
                    .h(px(chrome::TITLE_ROW_HEIGHT))
                    .w_full()
                    .flex()
                    .justify_end()
                    .child(chrome::window_controls(window, strings, palette, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px(32.0))
                    .child(
                        div()
                            .id("managed-host-recovery")
                            .accessibility_id("managed-host-recovery")
                            .role(Role::Group)
                            .aria_label(title.clone())
                            .w_full()
                            .max_w(px(448.0))
                            .flex()
                            .flex_col()
                            .gap(px(24.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .gap(px(12.0))
                                    .text_center()
                                    .child(
                                        div()
                                            .css_text(1.25, 1.75, cx)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .id("managed-host-error")
                                            .accessibility_id("managed-host-error")
                                            .role(Role::Status)
                                            .aria_label(description.clone())
                                            .max_w(px(384.0))
                                            .css_text(0.875, 1.25, cx)
                                            .text_color(palette.muted_foreground)
                                            .child(description),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(8.0))
                                    .child(retry)
                                    .child(remote),
                            ),
                    ),
            )
    }
}
