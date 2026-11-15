//! Recovery for a saved-host file that could not be loaded at startup.

use gpui_kit::{
    Context, FocusHandle, FontWeight, InteractiveElement, ParentElement, Role,
    StatefulInteractiveElement, Styled, Task, Window, div, px,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome;
use crate::host_profiles::{ProfileStore, ProfilesError};
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

enum RecoveryState {
    Failed(ProfilesError),
    Loading { _task: Task<()> },
    ChooseHost,
}

pub(crate) struct HostRecovery {
    store: ProfileStore,
    state: RecoveryState,
    focus: FocusHandle,
}

impl HostRecovery {
    pub(crate) fn new(
        store: ProfileStore,
        error: ProfilesError,
        cx: &mut Context<Workspace>,
    ) -> Self {
        Self {
            store,
            state: RecoveryState::Failed(error),
            focus: cx.focus_handle(),
        }
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut Context<Workspace>) {
        self.focus.focus(window, cx);
    }
}

impl Workspace {
    pub(crate) fn retry_saved_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(recovery) = self.host_recovery.as_ref() else {
            return;
        };
        if matches!(recovery.state, RecoveryState::Loading { .. }) {
            return;
        }
        let store = recovery.store.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { store.load() })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(recovery) = this.host_recovery.as_mut() else {
                    return;
                };
                match result {
                    Err(error) => recovery.state = RecoveryState::Failed(error),
                    Ok(profiles) => {
                        if let Some(profile) = profiles.default_profile() {
                            recovery.state = RecoveryState::Failed(ProfilesError::Unavailable);
                            let label = profile.label.clone();
                            this.connect_to_host(profile.endpoint.clone(), window, cx);
                            if let Some(connection) = this.connection.as_mut() {
                                connection.display_label = Some(label);
                            }
                        } else {
                            recovery.state = RecoveryState::ChooseHost;
                            this.open_host_picker(window, cx);
                        }
                    }
                }
                cx.notify();
            });
        });
        if let Some(recovery) = self.host_recovery.as_mut() {
            recovery.state = RecoveryState::Loading { _task: task };
        }
        cx.notify();
    }

    pub(crate) fn render_host_recovery(
        &self,
        palette: &Palette,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let recovery = self
            .host_recovery
            .as_ref()
            .expect("recovery view has recovery state");
        let strings = locale::strings(cx);
        let busy = matches!(recovery.state, RecoveryState::Loading { .. });
        let message = strings.get(match recovery.state {
            RecoveryState::Failed(ProfilesError::Invalid) => {
                "desktopHostSwitcher.error.invalidFile"
            }
            RecoveryState::Failed(_) => "desktopHostSwitcher.error.failedToLoad",
            RecoveryState::Loading { .. } => "desktopHostSwitcher.state.loading",
            RecoveryState::ChooseHost => "desktopHostSwitcher.description",
        });
        let retry = on_activate(
            gpui_kit::base::Button::new("host-startup-retry")
                .accessibility_id("host-startup-retry")
                .accessibility_label(strings.get("desktopHostSwitcher.actions.refresh"))
                .track_focus(&recovery.focus)
                .disabled(busy)
                .flex_shrink_0()
                .whitespace_nowrap()
                .h(px(32.0))
                .px(px(12.0))
                .rounded(px(7.0))
                .bg(palette.primary)
                .text_color(palette.primary_foreground)
                .child(strings.get("desktopHostSwitcher.actions.refresh")),
            cx,
            |this, window, cx| this.retry_saved_host(window, cx),
        );
        let choose = on_activate(
            gpui_kit::base::Button::new("host-startup-choose")
                .accessibility_id("host-startup-choose")
                .accessibility_label(strings.get("desktopHostSwitcher.actions.switchInstance"))
                .disabled(busy)
                .flex_shrink_0()
                .whitespace_nowrap()
                .h(px(32.0))
                .px(px(12.0))
                .rounded(px(7.0))
                .text_color(palette.foreground)
                .child(strings.get("desktopHostSwitcher.actions.switchInstance")),
            cx,
            move |this, window, cx| {
                if !busy {
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
                chrome::drag_region("host-recovery-titlebar", cx)
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
                            .id("host-startup-recovery")
                            .accessibility_id("host-startup-recovery")
                            .role(Role::Group)
                            .aria_label(strings.get("desktopHostSwitcher.title"))
                            .w_full()
                            .max_w(px(420.0))
                            .flex()
                            .flex_col()
                            .gap(px(16.0))
                            .text_center()
                            .child(
                                div()
                                    .css_text(1.25, 1.75, cx)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(strings.get("desktopHostSwitcher.title")),
                            )
                            .child(
                                div()
                                    .id("host-startup-error")
                                    .accessibility_id("host-startup-error")
                                    .role(Role::Status)
                                    .aria_label(message.clone())
                                    .a11y_synthetic_children(|builder| {
                                        builder
                                            .parent_node()
                                            .set_live(gpui_kit::accesskit::Live::Polite)
                                    })
                                    .min_h(px(48.0))
                                    .css_text(0.8125, 1.25, cx)
                                    .text_color(palette.muted_foreground)
                                    .child(message),
                            )
                            .child(
                                div()
                                    .flex_row_from(strings.direction())
                                    .justify_center()
                                    .gap(px(8.0))
                                    .child(retry)
                                    .child(choose),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, ElementId, TestAppContext};

    use super::*;
    use crate::workspace::Source;

    #[gpui_kit::test]
    fn invalid_host_startup_has_no_fixture_sessions_and_retry_preserves_the_file(
        cx: &mut TestAppContext,
    ) {
        let fixture = crate::recovery_tests::Fixture::new();
        let directory = fixture
            .journal()
            .parent()
            .expect("profile folder")
            .to_path_buf();
        fs::create_dir_all(&directory).expect("private folder");
        let file = directory.join("hosts.json");
        fs::write(&file, b"invalid fixture").expect("invalid profile");
        let (window, workspace) = crate::workspace_tests::open_source(
            cx,
            Source::HostRecovery {
                store: ProfileStore::new(&directory),
                error: ProfilesError::Invalid,
            },
        );
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                assert!(
                    workspace.sessions.active().is_empty(),
                    "recovery cannot load synthetic sessions"
                );
                assert!(!workspace.is_synthetic());
                assert!(!workspace.persistence_ready());
                workspace.retry_saved_host(window, cx);
            });
        })
        .expect("window");
        let started = Instant::now();
        loop {
            cx.run_until_parked();
            if cx.read(|cx| {
                matches!(
                    workspace
                        .read(cx)
                        .host_recovery
                        .as_ref()
                        .expect("recovery")
                        .state,
                    RecoveryState::Failed(ProfilesError::Invalid)
                )
            }) {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "read must settle"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(fs::read(&file).expect("retained file"), b"invalid fixture");
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window
                    .try_find(ElementId::Name("host-startup-retry".into()))
                    .is_some()
            );
            assert!(
                window
                    .try_find(ElementId::Name("composer".into()))
                    .is_none()
            );
        })
        .expect("window");
    }
}
