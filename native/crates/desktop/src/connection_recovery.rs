//! The recovery screen for a server this window never reached
//! (`DesktopConnectionRecovery.tsx` with `desktopRecoveryConfig.ts`).
//!
//! Electron boots into it when the chosen host cannot be used: unreachable,
//! not an OpenChamber server, or an OpenChamber this app cannot talk to. Once
//! a connection has been established, a lost server keeps the last valid
//! state on screen instead, as the reference's loaded page does. Native has no
//! local host to fall back to, so the screen offers Retry and Use Remote only,
//! as the reference does when local is unavailable.

use gpui_kit::component::Theme;
use gpui_kit::{
    Context, FontWeight, InteractiveElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{self, CONTROL_RADIUS, glyph};
use crate::connection::{ConnectionFailure, ConnectionState};
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The reference's `RecoveryVariant`s a remote host can end in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryVariant {
    Unreachable,
    WrongService,
    Incompatible,
}

impl RecoveryVariant {
    /// The variant a failed first connection shows, if any. Authentication
    /// failures have their own gate.
    pub(crate) fn of(failure: ConnectionFailure) -> Option<Self> {
        match failure {
            ConnectionFailure::Unreachable => Some(Self::Unreachable),
            ConnectionFailure::NotOpenChamber => Some(Self::WrongService),
            ConnectionFailure::Incompatible => Some(Self::Incompatible),
            _ => None,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Unreachable => "onboarding.desktopRecovery.remoteUnreachable.title",
            Self::WrongService => "onboarding.desktopRecovery.incompatibleServer.title",
            Self::Incompatible => "onboarding.desktopRecovery.remoteIncompatible.title",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Unreachable => "onboarding.desktopRecovery.remoteUnreachable.description",
            Self::WrongService => "onboarding.desktopRecovery.incompatibleServer.description",
            Self::Incompatible => "onboarding.desktopRecovery.remoteIncompatible.description",
        }
    }

    /// A wrong service will not become OpenChamber by trying again.
    fn retries(self) -> bool {
        self != Self::WrongService
    }
}

impl Workspace {
    /// The variant to show instead of the workspace, while the current
    /// connection has never been established.
    pub(crate) fn connection_recovery(&self) -> Option<RecoveryVariant> {
        self.connection
            .as_ref()
            .filter(|connection| !connection.established)
            .and_then(|connection| connection.recovery)
            .filter(|_| !self.authentication.visible)
    }

    /// Retry Connection: one attempt now, in place of a scheduled retry.
    pub(crate) fn retry_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        if connection.state == ConnectionState::Connecting {
            return;
        }
        connection.retry_task = None;
        self.connect(window, cx);
        cx.notify();
    }

    pub(crate) fn render_connection_recovery(
        &self,
        variant: RecoveryVariant,
        palette: &Palette,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let connection = self
            .connection
            .as_ref()
            .expect("recovery belongs to a connection");
        let strings = locale::strings(cx);
        let direction = strings.direction();
        let retrying = connection.state == ConnectionState::Connecting;
        let url = connection.config.endpoint.as_str().to_owned();
        // `formatHostDisplay`: the saved label, else the address.
        let host = connection
            .display_label
            .clone()
            .filter(|label| !label.trim().is_empty())
            .unwrap_or_else(|| url.clone());
        let description = strings.format(variant.description(), &[("host", &host)]);
        let title = strings.get(variant.title());
        let mono = Theme::global(cx).mono_font_family.clone();
        let button = |id: &'static str, icon: &'static str, label: SharedString, primary: bool| {
            let text = if primary {
                palette.primary
            } else {
                palette.foreground
            };
            gpui_kit::base::Button::new(id)
                .accessibility_id(id)
                .accessibility_label(label.clone())
                .disabled(retrying)
                .h(px(32.0))
                .px(px(10.0))
                .rounded(px(CONTROL_RADIUS))
                .border_1()
                .map(|this| {
                    if primary {
                        this.bg(palette.primary.opacity(0.12))
                            .border_color(palette.primary.opacity(0.16))
                    } else {
                        this.bg(palette.background)
                            .border_color(palette.border)
                            .hover(|style| style.bg(palette.hover))
                    }
                })
                .text_color(text)
                .css_text(0.843_75, 1.25, cx)
                .font_weight(FontWeight::MEDIUM)
                .child(
                    div()
                        .flex_row_from(direction)
                        .items_center()
                        .justify_center()
                        .gap(px(8.0))
                        .child(glyph(icon, 16.0, text))
                        .child(label),
                )
        };
        let retry = variant.retries().then(|| {
            let label = strings.get(if retrying {
                "onboarding.desktopRecovery.actions.retrying"
            } else {
                "onboarding.desktopRecovery.remoteUnreachable.retry"
            });
            on_activate(
                button("connection-recovery-retry", "refresh", label, true).w_full(),
                cx,
                |this, window, cx| this.retry_connection(window, cx),
            )
        });
        let use_remote = on_activate(
            button(
                "connection-recovery-use-remote",
                "server",
                strings.get("onboarding.desktopRecovery.common.useRemote"),
                false,
            )
            .flex_1(),
            cx,
            move |this, window, cx| {
                if !retrying {
                    this.open_host_picker(window, cx);
                }
            },
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
                chrome::drag_region("connection-recovery-titlebar", cx)
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
                            .id("connection-recovery")
                            .accessibility_id("connection-recovery")
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
                                        // The reference fades the whole badge,
                                        // its icon included.
                                        div()
                                            .p(px(12.0))
                                            .rounded_full()
                                            .bg(palette.status_warning)
                                            .opacity(0.15)
                                            .child(glyph("server", 32.0, palette.status_warning)),
                                    )
                                    .child(
                                        div()
                                            .id("connection-recovery-title")
                                            .role(Role::Heading)
                                            .css_text(1.25, 1.75, cx)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .id("connection-recovery-description")
                                            .accessibility_id("connection-recovery-description")
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
                                    .rounded(px(8.0))
                                    .border_1()
                                    .border_color(palette.border)
                                    .bg(palette.background.opacity(0.5))
                                    .p(px(12.0))
                                    .child(
                                        div()
                                            .mb(px(4.0))
                                            .css_text(0.75, 1.0, cx)
                                            .text_color(palette.muted_foreground)
                                            .child(strings.get(
                                                "onboarding.remoteConnection.field.serverAddress",
                                            )),
                                    )
                                    .child(
                                        div()
                                            .id("connection-recovery-address")
                                            .accessibility_id("connection-recovery-address")
                                            .truncate()
                                            .font_family(mono)
                                            .css_text(0.875, 1.25, cx)
                                            .child(url),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(8.0))
                                    .children(retry)
                                    .child(
                                        div()
                                            .flex_row_from(direction)
                                            .gap(px(8.0))
                                            .child(use_remote),
                                    ),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, ElementId, TestAppContext};
    use openchamber_native_transport::client::ClientDescription;
    use openchamber_native_transport::endpoint::Endpoint;

    use super::*;
    use crate::connection::{Connection, ServerConfig};

    #[gpui_kit::test]
    fn a_server_never_reached_shows_recovery_until_a_connection_is_established(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free port")
            .port();
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let endpoint =
                    Endpoint::parse(&format!("http://127.0.0.1:{port}/")).expect("endpoint");
                let config = ServerConfig {
                    endpoint,
                    password: None,
                    client: ClientDescription::default(),
                };
                workspace.connection =
                    Some(Connection::new(config).expect("test network runtime starts"));
                workspace.connect(window, cx);
            })
        })
        .expect("window");
        let started = Instant::now();
        while cx.read(|cx| workspace.read(cx).connection_recovery()).is_none() {
            cx.run_until_parked();
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "a closed port fails"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        cx.update_window(window, |_, window, cx| {
            assert_eq!(
                workspace.read(cx).connection_recovery(),
                Some(RecoveryVariant::Unreachable)
            );
            window.render_frame(cx);
            for present in [
                "connection-recovery-retry",
                "connection-recovery-use-remote",
            ] {
                assert!(
                    window.try_find(ElementId::Name(present.into())).is_some(),
                    "{present} is drawn"
                );
            }
            assert!(
                window
                    .try_find(ElementId::Name("composer-submit".into()))
                    .is_none(),
                "no workspace behind the recovery screen"
            );
            workspace.update(cx, |workspace, cx| {
                workspace.retry_connection(window, cx);
                let connection = workspace.connection.as_ref().expect("connection");
                assert_eq!(connection.state, ConnectionState::Connecting);
                assert!(connection.retry_task.is_none(), "Retry replaces the timer");
                assert_eq!(
                    workspace.connection_recovery(),
                    Some(RecoveryVariant::Unreachable),
                    "the screen stays while retrying"
                );
                // Once a connection was established, a lost server keeps
                // the last valid state on screen.
                workspace.connection.as_mut().expect("connection").established = true;
                assert_eq!(workspace.connection_recovery(), None);
            });
        })
        .expect("window");
    }

    #[test]
    fn variants_follow_the_reference_configuration() {
        assert_eq!(
            RecoveryVariant::of(ConnectionFailure::Unreachable),
            Some(RecoveryVariant::Unreachable)
        );
        assert_eq!(
            RecoveryVariant::of(ConnectionFailure::NotOpenChamber),
            Some(RecoveryVariant::WrongService)
        );
        assert_eq!(
            RecoveryVariant::of(ConnectionFailure::Incompatible),
            Some(RecoveryVariant::Incompatible)
        );
        assert_eq!(
            RecoveryVariant::of(ConnectionFailure::AuthenticationRequired),
            None
        );
        assert!(RecoveryVariant::Unreachable.retries());
        assert!(RecoveryVariant::Incompatible.retries());
        assert!(!RecoveryVariant::WrongService.retries());
    }
}
