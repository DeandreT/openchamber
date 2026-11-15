//! The OpenCode compatibility gate (`OpenCodeCompatibilityGate.tsx`).
//!
//! The workspace keeps starting while the check runs; only a confirmed
//! incompatible OpenCode replaces it with this screen. A failed check leaves
//! the connection's own explanations in charge and never dismisses a known
//! incompatible answer. The gate belongs to its connection, so switching
//! hosts drops it with the old connection.

use gpui_kit::component::Theme;
use gpui_kit::{
    Context, FontWeight, InteractiveElement, ParentElement, Role, StatefulInteractiveElement,
    Styled, Window, div, prelude::FluentBuilder, px, svg,
};
use openchamber_native_transport::compatibility::{
    CompatibilityState, Installation, OpenCodeCompatibility,
};
use openchamber_native_transport::error::TransportError;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{self, CONTROL_RADIUS, glyph};
use crate::locale::{self, Strings};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

const BUNDLED_GUIDE: &str = "https://github.com/openchamber/openchamber/releases/latest";
const OPENCODE_GUIDE: &str = "https://opencode.ai/download";

/// What the gate is doing for the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    InstallV2,
    Reconnect,
}

/// The connection's compatibility answer and the gate's recovery state.
#[derive(Debug, Default)]
pub(crate) struct CompatibilityGate {
    /// The last answer that said compatible or incompatible.
    known: Option<OpenCodeCompatibility>,
    pub(crate) operation: Option<Operation>,
    pub(crate) failed: bool,
    /// Bumps per check or recovery; older results are dropped.
    pub(crate) revision: u64,
}

impl CompatibilityGate {
    /// The answer to show, when the host's OpenCode is confirmed incompatible.
    pub(crate) fn blocking(&self) -> Option<&OpenCodeCompatibility> {
        self.known
            .as_ref()
            .filter(|known| known.state == CompatibilityState::Incompatible)
    }

    /// Takes a check's answer. Failed reads and `unavailable` answers keep
    /// what was known.
    pub(crate) fn apply_check(&mut self, result: Result<OpenCodeCompatibility, TransportError>) {
        if let Ok(answer) = result
            && answer.state != CompatibilityState::Unavailable
        {
            self.known = Some(answer);
        }
    }
}

/// The gate's text for one answer, as the reference picks it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GateCopy {
    pub(crate) title: &'static str,
    pub(crate) description: &'static str,
    pub(crate) install: &'static str,
    /// What the version pill points to: `2.x` or `<minimum>+`.
    pub(crate) target: String,
    pub(crate) guide: &'static str,
}

pub(crate) fn gate_copy(answer: &OpenCodeCompatibility) -> GateCopy {
    let external = answer.installation == Installation::External;
    let bundled = answer.installation == Installation::Bundled;
    let outdated = answer.outdated();
    let description = match (bundled, outdated, external) {
        (true, _, _) => "opencodeCompatibility.bundled",
        (false, true, true) => "opencodeCompatibility.outdatedExternal",
        (false, true, false) => "opencodeCompatibility.outdatedLocal",
        (false, false, true) => "opencodeCompatibility.external",
        (false, false, false) => "opencodeCompatibility.local",
    };
    GateCopy {
        title: if outdated {
            "opencodeCompatibility.outdatedTitle"
        } else {
            "opencodeCompatibility.title"
        },
        description,
        install: if outdated {
            "opencodeCompatibility.update"
        } else {
            "opencodeCompatibility.install"
        },
        target: match (&answer.minimum_version, outdated) {
            (Some(minimum), true) => format!("{minimum}+"),
            _ => "2.x".to_owned(),
        },
        guide: if bundled {
            BUNDLED_GUIDE
        } else {
            OPENCODE_GUIDE
        },
    }
}

fn description(strings: &Strings, answer: &OpenCodeCompatibility, copy: &GateCopy) -> String {
    let text = strings.format(
        copy.description,
        &[
            ("version", answer.version.as_deref().unwrap_or_default()),
            (
                "minimum",
                answer.minimum_version.as_deref().unwrap_or_default(),
            ),
        ],
    );
    if answer.can_install {
        format!(
            "{text} {}",
            strings.get("opencodeCompatibility.installDescription")
        )
    } else {
        text.to_string()
    }
}

impl Workspace {
    /// Checks the connection's OpenCode once it is ready.
    pub(crate) fn check_compatibility(
        &mut self,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self
            .connection
            .as_mut()
            .filter(|connection| connection.generation == generation)
        else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        connection.compatibility.revision += 1;
        let revision = connection.compatibility.revision;
        let task = connection
            .network
            .run(async move { client.opencode_compatibility().await });
        connection.compatibility_task = Some(cx.spawn_in(window, async move |this, cx| {
            let Some(result) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let Some(connection) = this.connection.as_mut().filter(|connection| {
                    connection.generation == generation
                        && connection.compatibility.revision == revision
                }) else {
                    return;
                };
                if let Err(error) = &result {
                    tracing::warn!(%error, "the OpenCode compatibility check failed");
                }
                connection.compatibility.apply_check(result);
                cx.notify();
            });
        }));
    }

    /// Install v2 or Check again: a recovered host reconnects from the start.
    pub(crate) fn recover_opencode(
        &mut self,
        operation: Operation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let Some(client) = connection.client.clone() else {
            return;
        };
        if connection.compatibility.operation.is_some() {
            return;
        }
        connection.compatibility.revision += 1;
        let revision = connection.compatibility.revision;
        connection.compatibility.operation = Some(operation);
        connection.compatibility.failed = false;
        let endpoint = connection.config.endpoint.clone();
        let task = connection.network.run(async move {
            match operation {
                Operation::InstallV2 => client.install_opencode_v2().await.map(|()| None),
                Operation::Reconnect => {
                    let answer = client.opencode_compatibility().await?;
                    match answer.state {
                        CompatibilityState::Incompatible => Ok(Some(answer)),
                        CompatibilityState::Unavailable => Err(TransportError::Malformed(
                            "the host could not determine OpenCode compatibility".to_owned(),
                        )),
                        CompatibilityState::Compatible => {
                            client.reload_opencode().await.map(|()| None)
                        }
                    }
                }
            }
        });
        connection.compatibility_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await.unwrap_or(Err(TransportError::Cancelled));
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(connection) = this.connection.as_mut().filter(|connection| {
                    connection.config.endpoint == endpoint
                        && connection.compatibility.revision == revision
                }) else {
                    return;
                };
                let gate = &mut connection.compatibility;
                gate.operation = None;
                match result {
                    Ok(Some(answer)) => gate.apply_check(Ok(answer)),
                    // The page reload of the reference: the workspace reads
                    // the host again from the start.
                    Ok(None) => {
                        *gate = CompatibilityGate::default();
                        this.connect(window, cx);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the OpenCode recovery failed");
                        gate.failed = true;
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn render_compatibility_gate(
        &self,
        palette: &Palette,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<gpui_kit::Stateful<gpui_kit::Div>> {
        let gate = &self.connection.as_ref()?.compatibility;
        let answer = gate.blocking()?;
        let strings = locale::strings(cx);
        let direction = strings.direction();
        let copy = gate_copy(answer);
        let busy = gate.operation.is_some();
        let mono = Theme::global(cx).mono_font_family.clone();
        let button = |id: &'static str,
                      icon: &'static str,
                      label: gpui_kit::SharedString,
                      primary: bool,
                      operation: Operation| {
            let running = gate.operation == Some(operation);
            let (fill, text) = if primary {
                (Some(palette.primary), palette.primary_foreground)
            } else {
                (None, palette.foreground)
            };
            on_activate(
                gpui_kit::base::Button::new(id)
                    .accessibility_id(id)
                    .accessibility_label(label.clone())
                    .disabled(busy)
                    .h(px(32.0))
                    .px(px(12.0))
                    .rounded(px(CONTROL_RADIUS))
                    .when_some(fill, |this, fill| this.bg(fill))
                    .when(!primary, |this| this.hover(|style| style.bg(palette.hover)))
                    .text_color(text)
                    .css_text(0.875, 1.25, cx)
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(8.0))
                            .child(glyph(if running { "refresh" } else { icon }, 16.0, text))
                            .child(label),
                    ),
                cx,
                move |this, window, cx| this.recover_opencode(operation, window, cx),
            )
        };
        let reconnect_label = strings.get(if gate.operation == Some(Operation::Reconnect) {
            "opencodeCompatibility.reconnecting"
        } else {
            "opencodeCompatibility.reconnect"
        });
        let install_label = strings.get(if gate.operation == Some(Operation::InstallV2) {
            "opencodeCompatibility.installing"
        } else {
            copy.install
        });
        let guide = copy.guide;
        let muted = palette.muted_foreground;
        let foreground = palette.foreground;
        Some(
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
                    chrome::drag_region("compatibility-titlebar", cx)
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
                        .px(px(24.0))
                        .py(px(40.0))
                        .child(
                            div()
                                .id("opencode-compatibility")
                                .accessibility_id("opencode-compatibility")
                                .role(Role::Group)
                                .aria_label(strings.get(copy.title))
                                .w_full()
                                .max_w(px(384.0))
                                .flex()
                                .flex_col()
                                .items_center()
                                .text_center()
                                .child(
                                    svg()
                                        .path(crate::assets::APP_LOGO)
                                        .size(px(48.0))
                                        .text_color(palette.foreground),
                                )
                                .child(
                                    div()
                                        .id("opencode-compatibility-title")
                                        .role(Role::Heading)
                                        .mt(px(24.0))
                                        .css_text(1.25, 1.75, cx)
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(strings.get(copy.title)),
                                )
                                .child(
                                    div()
                                        .mt(px(8.0))
                                        .css_text(0.875, 1.421_875, cx)
                                        .text_color(palette.muted_foreground)
                                        .child(description(strings, answer, &copy)),
                                )
                                .when_some(answer.version.clone(), |this, version| {
                                    this.child(
                                        div()
                                            .mt(px(20.0))
                                            .px(px(10.0))
                                            .py(px(4.0))
                                            .rounded(px(CONTROL_RADIUS))
                                            .border_1()
                                            .border_color(palette.border)
                                            .font_family(mono.clone())
                                            .css_text(0.75, 1.0, cx)
                                            .flex()
                                            .items_center()
                                            .gap(px(8.0))
                                            .child(div().text_color(muted).child(version))
                                            .child(glyph("arrow-right", 14.0, muted))
                                            .child(div().text_color(foreground).child(copy.target)),
                                    )
                                })
                                .when_some(answer.binary.clone(), |this, binary| {
                                    this.child(
                                        div()
                                            .mt(px(8.0))
                                            .max_w_full()
                                            .font_family(mono.clone())
                                            .css_text(0.75, 1.0, cx)
                                            .text_color(muted)
                                            .child(binary),
                                    )
                                })
                                .when(gate.failed, |this| {
                                    this.child(
                                        div()
                                            .id("opencode-compatibility-failed")
                                            .role(Role::Alert)
                                            .mt(px(20.0))
                                            .px(px(12.0))
                                            .py(px(8.0))
                                            .rounded(px(CONTROL_RADIUS))
                                            .border_1()
                                            .border_color(palette.error.opacity(0.3))
                                            .bg(palette.error.opacity(0.1))
                                            .text_color(palette.error)
                                            .css_text(0.875, 1.25, cx)
                                            .child(strings.get("opencodeCompatibility.failed")),
                                    )
                                })
                                .child(
                                    div()
                                        .mt(px(28.0))
                                        .flex_row_from(direction)
                                        .flex_wrap()
                                        .justify_center()
                                        .gap(px(8.0))
                                        .when(answer.can_install, |this| {
                                            this.child(button(
                                                "opencode-install",
                                                "download",
                                                install_label,
                                                true,
                                                Operation::InstallV2,
                                            ))
                                        })
                                        .child(button(
                                            "opencode-reconnect",
                                            "refresh",
                                            reconnect_label,
                                            !answer.can_install,
                                            Operation::Reconnect,
                                        )),
                                )
                                .child(on_activate(
                                    div()
                                        .id("opencode-guide")
                                        .role(Role::Link)
                                        .accessibility_id("opencode-guide")
                                        .aria_label(strings.get("opencodeCompatibility.guide"))
                                        .mt(px(24.0))
                                        .flex_row_from(direction)
                                        .items_center()
                                        .gap(px(4.0))
                                        .cursor_pointer()
                                        .css_text(0.75, 1.0, cx)
                                        .text_color(muted)
                                        .hover(move |style| style.text_color(foreground))
                                        .child(strings.get("opencodeCompatibility.guide"))
                                        .child(glyph("external-link", 12.0, muted)),
                                    cx,
                                    move |_, _, cx| cx.open_url(guide),
                                ))
                                .child(on_activate(
                                    gpui_kit::base::Button::new("opencode-switch-host")
                                        .accessibility_id("opencode-switch-host")
                                        .accessibility_label(
                                            strings
                                                .get("desktopHostSwitcher.actions.switchInstance"),
                                        )
                                        .mt(px(12.0))
                                        .h(px(32.0))
                                        .px(px(12.0))
                                        .rounded(px(CONTROL_RADIUS))
                                        .hover(|style| style.bg(palette.hover))
                                        .text_color(palette.foreground)
                                        .css_text(0.875, 1.25, cx)
                                        .child(
                                            strings
                                                .get("desktopHostSwitcher.actions.switchInstance"),
                                        ),
                                    cx,
                                    |this, window, cx| this.open_host_picker(window, cx),
                                )),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(
        version: &str,
        installation: Installation,
        minimum: Option<&str>,
    ) -> OpenCodeCompatibility {
        OpenCodeCompatibility {
            state: CompatibilityState::Incompatible,
            version: Some(version.to_owned()),
            installation,
            binary: None,
            minimum_version: minimum.map(str::to_owned),
            can_install: installation == Installation::Managed,
        }
    }

    #[test]
    fn copy_follows_installation_and_version() {
        let v1_external = gate_copy(&answer("1.4.0", Installation::External, None));
        assert_eq!(v1_external.title, "opencodeCompatibility.title");
        assert_eq!(v1_external.description, "opencodeCompatibility.external");
        assert_eq!(v1_external.target, "2.x");
        assert_eq!(v1_external.guide, OPENCODE_GUIDE);
        let outdated = gate_copy(&answer("2.0.1", Installation::Managed, Some("2.1.0")));
        assert_eq!(outdated.title, "opencodeCompatibility.outdatedTitle");
        assert_eq!(outdated.description, "opencodeCompatibility.outdatedLocal");
        assert_eq!(outdated.install, "opencodeCompatibility.update");
        assert_eq!(outdated.target, "2.1.0+");
        let bundled = gate_copy(&answer("2.0.1", Installation::Bundled, Some("2.1.0")));
        assert_eq!(bundled.description, "opencodeCompatibility.bundled");
        assert_eq!(bundled.guide, BUNDLED_GUIDE);
    }

    #[test]
    fn failed_checks_never_dismiss_a_known_incompatibility() {
        let mut gate = CompatibilityGate::default();
        assert!(gate.blocking().is_none());
        gate.apply_check(Ok(answer("1.4.0", Installation::External, None)));
        assert!(gate.blocking().is_some());
        gate.apply_check(Err(TransportError::Cancelled));
        assert!(gate.blocking().is_some(), "a failed read keeps the gate");
        let mut unavailable = answer("1.4.0", Installation::External, None);
        unavailable.state = CompatibilityState::Unavailable;
        gate.apply_check(Ok(unavailable));
        assert!(
            gate.blocking().is_some(),
            "an unknown answer keeps the gate"
        );
        let mut compatible = answer("2.1.0", Installation::External, None);
        compatible.state = CompatibilityState::Compatible;
        gate.apply_check(Ok(compatible));
        assert!(gate.blocking().is_none());
    }

    #[test]
    fn description_appends_the_install_note_only_when_the_host_can_install() {
        let strings = Strings::load("en");
        let managed = answer("1.4.0", Installation::Managed, None);
        let text = description(&strings, &managed, &gate_copy(&managed));
        assert!(text.contains("1.4.0"));
        assert!(text.ends_with(&*strings.get("opencodeCompatibility.installDescription")));
        let external = answer("1.4.0", Installation::External, None);
        let text = description(&strings, &external, &gate_copy(&external));
        assert!(!text.contains(&*strings.get("opencodeCompatibility.installDescription")));
    }
}
