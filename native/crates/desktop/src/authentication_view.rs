//! The native sign-in gate. It uses the shared authentication labels and keeps
//! password text inside a secret input with no accessibility value.

use gpui_kit::component::input::{Input, InputContentType};
use gpui_kit::{
    Context, FontWeight, InteractiveElement, ParentElement, Role, StatefulInteractiveElement,
    Styled, TestSupportExt, Toggled, Window, div, prelude::FluentBuilder, px,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome;
use crate::connection::ConnectionFailure;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

impl Workspace {
    pub(crate) fn render_authentication(
        &self,
        palette: &Palette,
        direction: Direction,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let strings = locale::strings(cx);
        let auth = &self.authentication;
        let host = self
            .connection
            .as_ref()
            .map(|connection| connection.config.endpoint.authority())
            .unwrap_or_default();
        let token_only = auth.method == crate::authentication::SignInMethod::Pairing;
        let disabled = auth.submitting
            || auth.password.read(cx).value().is_empty()
            || token_only
            || auth
                .retry_not_before
                .is_some_and(|deadline| deadline > std::time::Instant::now());
        let message = match auth.failure {
            Some(
                ConnectionFailure::AuthenticationRequired
                | ConnectionFailure::ClientAuthenticationRequired,
            )
            | None
                if auth.storage_error.is_some() =>
            {
                strings.get("sessionAuth.error.savedSignInUnavailable")
            }
            Some(
                ConnectionFailure::AuthenticationRequired
                | ConnectionFailure::ClientAuthenticationRequired,
            )
            | None => "".into(),
            Some(failure) => crate::sidebar_view::failure_text(strings, failure, &host),
        };
        let unlock = on_activate(
            div()
                .id("authentication-unlock")
                .accessibility_id("authentication-unlock")
                .role(Role::Button)
                .aria_label(strings.get(if auth.submitting {
                    "sessionAuth.actions.unlockingAria"
                } else {
                    "sessionAuth.actions.unlockAria"
                }))
                .when(disabled, |this| {
                    this.a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
                })
                .track_focus(&auth.unlock_focus)
                .tab_index(0)
                .size(px(32.0))
                .flex_shrink_0()
                .rounded(px(7.0))
                .flex()
                .items_center()
                .justify_center()
                .bg(palette.primary)
                .text_color(palette.primary_foreground)
                .when(disabled, |this| this.opacity(0.5))
                .when(!disabled, |this| {
                    this.cursor_pointer()
                        .hover(|style| style.bg(palette.primary_hover))
                })
                .child(chrome::glyph(
                    "lock-unlock",
                    16.0,
                    palette.primary_foreground,
                )),
            cx,
            |this, window, cx| this.submit_authentication(window, cx),
        )
        .test_support();
        let trust = on_activate(
            div()
                .id("authentication-trust")
                .accessibility_id("authentication-trust")
                .role(Role::CheckBox)
                .aria_label(strings.get("sessionAuth.actions.trustDeviceAria"))
                .aria_toggled(if auth.trust_device {
                    Toggled::True
                } else {
                    Toggled::False
                })
                .when(auth.submitting, |this| {
                    this.a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
                })
                .track_focus(&auth.trust_focus)
                .tab_index(0)
                .flex_row_from(direction)
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .css_text(0.75, 1.0, cx)
                .text_color(palette.muted_foreground)
                .cursor_pointer()
                .child(chrome::glyph(
                    if auth.trust_device {
                        "checkbox"
                    } else {
                        "checkbox-blank"
                    },
                    16.0,
                    palette.muted_foreground,
                ))
                .child(strings.get("sessionAuth.actions.trustDevice")),
            cx,
            |this, _, cx| {
                if !this.authentication.submitting {
                    this.authentication.trust_device = !this.authentication.trust_device;
                    cx.notify();
                }
            },
        )
        .test_support();
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
                chrome::drag_region("authentication-titlebar", cx)
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
                    .p(px(24.0))
                    .child(
                        div()
                            .id("authentication-panel")
                            .accessibility_id("authentication-panel")
                            .role(Role::Group)
                            .aria_label(strings.get("sessionAuth.locked.unlockTitle"))
                            .w_full()
                            .max_w(px(320.0))
                            .flex()
                            .flex_col()
                            .gap(px(24.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .text_center()
                                    .child(
                                        div()
                                            .css_text(1.25, 1.75, cx)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(strings.get(if token_only {
                                                "sessionAuth.locked.tunnelTitle"
                                            } else {
                                                "sessionAuth.locked.unlockTitle"
                                            })),
                                    )
                                    .child(
                                        div()
                                            .css_text(0.8125, 1.25, cx)
                                            .text_color(palette.muted_foreground)
                                            .child(strings.get(if token_only {
                                                "sessionAuth.locked.tunnelDescription"
                                            } else {
                                                "sessionAuth.locked.passwordDescription"
                                            })),
                                    ),
                            )
                            .when(!token_only, |this| {
                                this.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap(px(8.0))
                                        .child(
                                            div()
                                                .flex_row_from(direction)
                                                .items_center()
                                                .gap(px(8.0))
                                                .child(
                                                    div().flex_1().min_w_0().child(
                                                        Input::new(&auth.password)
                                                            .id("authentication-password")
                                                            .accessibility_id(
                                                                "authentication-password",
                                                            )
                                                            .content_type(
                                                                InputContentType::Password,
                                                            )
                                                            .focus_bordered(false)
                                                            .when(
                                                                direction == Direction::RightToLeft,
                                                                |input| input.text_right(),
                                                            )
                                                            .aria_label(strings.get(
                                                                "sessionAuth.password.placeholder",
                                                            ))
                                                            .disabled(auth.submitting),
                                                    ),
                                                )
                                                .child(unlock),
                                        )
                                        .child(trust),
                                )
                            })
                            .child(
                                div()
                                    .id("authentication-error")
                                    .accessibility_id("authentication-error")
                                    .role(Role::Status)
                                    .a11y_synthetic_children(|builder| {
                                        builder
                                            .parent_node()
                                            .set_live(gpui_kit::accesskit::Live::Polite)
                                    })
                                    .aria_label(message.clone())
                                    .h(px(40.0))
                                    .css_text(0.75, 1.0, cx)
                                    .text_center()
                                    .text_color(palette.error)
                                    .child(message),
                            )
                            .child(on_activate(
                                gpui_kit::base::Button::new("authentication-switch-instance")
                                    .accessibility_id("authentication-switch-instance")
                                    .accessibility_label(
                                        strings.get("desktopHostSwitcher.actions.switchInstance"),
                                    )
                                    .h(px(28.0))
                                    .px(px(8.0))
                                    .text_color(palette.muted_foreground)
                                    .child(
                                        strings.get("desktopHostSwitcher.actions.switchInstance"),
                                    ),
                                cx,
                                |this, window, cx| this.open_host_picker(window, cx),
                            )),
                    ),
            )
    }
}
