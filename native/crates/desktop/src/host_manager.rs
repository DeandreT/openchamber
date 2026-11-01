//! The native direct-host list and its non-secret profile editor.

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Role,
    SharedString, StatefulInteractiveElement, Styled, Task, WeakEntity, Window, div,
    prelude::FluentBuilder, px,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::host_profiles::{HostProfile, HostProfiles, ProfileStore, ProfilesError};
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Presentation {
    Picker,
    Settings,
}

pub(crate) struct HostManager {
    owner: WeakEntity<Workspace>,
    store: Option<ProfileStore>,
    profiles: Option<HostProfiles>,
    task: Option<Task<()>>,
    pending_write: bool,
    closing: bool,
    error: Option<&'static str>,
    network: Option<crate::network::Network>,
    editing: Option<String>,
    label: Entity<InputState>,
    url: Entity<InputState>,
    presentation: Presentation,
    pub(crate) settings_scroll: gpui_kit::ScrollHandle,
}

impl HostManager {
    fn apply_profiles(&mut self, profiles: HostProfiles, cx: &mut Context<Self>) {
        let _ = self.owner.update(cx, |owner, cx| {
            if let Some(connection) = owner.connection.as_mut() {
                let label = profiles
                    .hosts
                    .iter()
                    .find(|host| host.endpoint == connection.config.endpoint)
                    .map(|host| host.label.clone());
                if connection.display_label != label {
                    connection.display_label = label;
                    cx.notify();
                }
            }
        });
        self.profiles = Some(profiles);
    }

    fn new(owner: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store =
            crate::install_id::config_directory().map(|directory| ProfileStore::new(&directory));
        Self {
            owner,
            store,
            profiles: None,
            task: None,
            pending_write: false,
            closing: false,
            error: None,
            network: crate::network::Network::new().ok(),
            editing: None,
            presentation: Presentation::Picker,
            settings_scroll: gpui_kit::ScrollHandle::new(),
            label: cx.new(|cx| {
                InputState::new(window, cx).placeholder(
                    locale::strings(cx).get("desktopHostSwitcher.field.labelOptionalPlaceholder"),
                )
            }),
            url: cx.new(|cx| {
                InputState::new(window, cx).placeholder(
                    locale::strings(cx).get("desktopHostSwitcher.field.urlPlaceholder"),
                )
            }),
        }
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        if self.task.is_some() || self.closing {
            return;
        }
        let Some(store) = self.store.clone() else {
            self.error = Some("desktopHostSwitcher.error.failedToLoad");
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { store.load() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.task = None;
                match result {
                    Ok(profiles) => {
                        this.apply_profiles(profiles, cx);
                        this.error = None;
                    }
                    Err(error) => {
                        this.error = Some(if error == ProfilesError::Invalid {
                            "desktopHostSwitcher.error.invalidFile"
                        } else {
                            "desktopHostSwitcher.error.failedToLoad"
                        });
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn edit(&mut self, profile: Option<&HostProfile>, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        if self.presentation == Presentation::Settings {
            self.settings_scroll
                .set_offset(gpui_kit::point(px(0.0), px(0.0)));
        }
        use std::hash::{BuildHasher, Hasher};
        self.editing = Some(
            profile
                .map(|profile| profile.id.clone())
                .unwrap_or_else(|| {
                    let mut hash = std::collections::hash_map::RandomState::new().build_hasher();
                    hash.write_u128(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos(),
                    );
                    format!("host_{:016x}", hash.finish())
                }),
        );
        self.label.update(cx, |input, cx| {
            input.set_value(profile.map_or("", |profile| &profile.label), window, cx)
        });
        self.url.update(cx, |input, cx| {
            input.set_value(
                profile.map_or("", |profile| profile.endpoint.as_str()),
                window,
                cx,
            );
            input.focus(window, cx);
        });
        self.error = None;
        cx.notify();
    }

    fn add_in_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || self.task.is_some() {
            return;
        }
        self.presentation = Presentation::Settings;
        self.edit(None, window, cx);
        crate::settings_view::open_hosts(cx.entity(), window, cx);
        self.url.update(cx, |input, cx| input.focus(window, cx));
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Some(mut next)) = (self.editing.clone(), self.profiles.clone()) else {
            return;
        };
        let url = self.url.read(cx).value().trim().to_owned();
        if openchamber_native_transport::endpoint::Endpoint::parse(&url).is_err() {
            self.error = Some("desktopHostSwitcher.error.invalidUrl");
            cx.notify();
            return;
        }
        let label = self.label.read(cx).value().trim().to_owned();
        let label = if label.is_empty() { &url } else { &label };
        if let Err(error) = next.upsert(&id, label, &url) {
            self.error = Some(save_error_key(error));
            cx.notify();
            return;
        }
        self.write(next, true, cx);
    }

    fn remove_profile(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.task.is_some() || self.closing {
            return;
        }
        let (Some(expected), Some(store), Some(network), Some(owner)) = (
            self.profiles.clone(),
            self.store.clone(),
            self.network.as_ref(),
            self.owner.upgrade(),
        ) else {
            self.error = Some("desktopHostSwitcher.error.failedToForget");
            cx.notify();
            return;
        };
        let Some(profile) = expected.hosts.iter().find(|profile| profile.id == id) else {
            return;
        };
        let endpoint = profile.endpoint.as_str().to_owned();
        let credentials = owner.read(cx).authentication.store.clone();
        let lease = match credentials.invalidate_endpoint(&endpoint) {
            Ok(lease) => lease,
            Err(_) => {
                self.error = Some("desktopHostSwitcher.error.failedToForget");
                cx.notify();
                return;
            }
        };
        owner.update(cx, |owner, _| {
            owner.authentication.remembered.remove(&endpoint);
        });
        let forgotten = network.run(async move { credentials.forget_endpoint(lease).await });
        let mut next = expected.clone();
        next.remove(id);
        let removed_id = id.to_owned();
        self.error = None;
        self.pending_write = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            if !matches!(forgotten.await, Some(Ok(()))) {
                let _ = this.update(cx, |this, cx| {
                    this.task = None;
                    this.error = Some("desktopHostSwitcher.error.failedToForget");
                    this.pending_write = false;
                    cx.notify();
                });
                return;
            }
            owner.update(cx, |owner, _| {
                owner.authentication.remembered.remove(&endpoint);
            });
            let result = cx
                .background_executor()
                .spawn(async move { store.save(&expected, &next) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.task = None;
                match result {
                    Ok(profiles) => {
                        this.apply_profiles(profiles, cx);
                        this.error = None;
                        if this.editing.as_deref() == Some(removed_id.as_str()) {
                            this.editing = None;
                        }
                    }
                    Err(error) => this.error = Some(save_error_key(error)),
                }
                this.pending_write = false;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn write(&mut self, next: HostProfiles, finish_edit: bool, cx: &mut Context<Self>) {
        if self.task.is_some() || self.closing {
            return;
        }
        let (Some(store), Some(expected)) = (self.store.clone(), self.profiles.clone()) else {
            return;
        };
        self.pending_write = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { store.save(&expected, &next) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.task = None;
                match result {
                    Ok(profiles) => {
                        this.apply_profiles(profiles, cx);
                        this.error = None;
                        if finish_edit {
                            this.editing = None;
                        }
                    }
                    Err(error) => this.error = Some(save_error_key(error)),
                }
                this.pending_write = false;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn connect(&self, profile: &HostProfile, window: &mut Window, cx: &mut Context<Self>) {
        let endpoint = profile.endpoint.clone();
        let label = profile.label.clone();
        let _ = self.owner.update(cx, |owner, cx| {
            owner.connect_to_host(endpoint.clone(), window, cx);
            if let Some(connection) = owner.connection.as_mut()
                && connection.config.endpoint == endpoint
            {
                connection.display_label = Some(label);
                cx.notify();
            }
        });
    }

    pub(crate) fn take_pending_write(&mut self) -> Option<Task<()>> {
        self.closing = true;
        if self.pending_write {
            self.task.take()
        } else {
            None
        }
    }

    pub(crate) fn finish_close(&mut self) -> bool {
        self.closing = false;
        self.error.is_none()
    }
}

impl Workspace {
    pub(crate) fn take_profile_flush(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.host_close.take().or_else(|| {
            self.host_manager
                .as_ref()
                .and_then(|manager| manager.update(cx, |manager, _| manager.take_pending_write()))
        })
    }

    pub(crate) fn unfreeze_host_manager(&mut self, cx: &mut Context<Self>) {
        if let Some(manager) = &self.host_manager {
            manager.update(cx, |manager, cx| {
                manager.finish_close();
                cx.notify();
            });
        }
    }

    pub(crate) fn open_host_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let owner = cx.weak_entity();
        let manager = self
            .host_manager
            .get_or_insert_with(|| cx.new(|cx| HostManager::new(owner, window, cx)))
            .clone();
        manager.update(cx, |manager, cx| {
            manager.presentation = Presentation::Picker;
            manager.load(cx);
        });
        window.open_dialog(cx, move |dialog, _, cx| {
            dialog
                .w(px(672.0))
                .title(locale::strings(cx).get("desktopHostSwitcher.title"))
                .child(
                    div()
                        .id("host-picker")
                        .accessibility_id("host-picker")
                        .role(Role::Group)
                        .child(manager.clone()),
                )
        });
    }

    pub(crate) fn connect_to_host(
        &mut self,
        endpoint: openchamber_native_transport::endpoint::Endpoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_unconnected = self.connection.is_none();
        self.save_composer_draft(cx);
        self.queue_persistence(cx);
        let config = crate::connection::ServerConfig::for_endpoint(endpoint);
        let mut connection = match crate::connection::Connection::new(config) {
            Ok(connection) => connection,
            Err(_) => {
                self.notify_mutation_failure("desktopHostSwitcher.error.failedToLoad", window, cx);
                return;
            }
        };
        connection.focus_on_ready = self.host_recovery.is_some();
        connection.generation = self
            .connection
            .as_ref()
            .map_or(0, |current| current.generation);
        self.clear_host_authentication(window, cx);
        if let Some(previous) = self.connection.take() {
            self.retired_networks
                .push((self.runtime.clone(), previous.into_network()));
        }
        self.connection = Some(connection);
        self.host_recovery = None;
        if was_unconnected {
            self.start_persistence(window, cx);
        }
        window.close_dialog(cx);
        self.connect(window, cx);
    }

    pub(crate) fn release_retired_networks(&mut self) {
        self.retired_networks.retain(|(runtime, network)| {
            !network.is_idle()
                || self
                    .mutation_tasks
                    .keys()
                    .chain(self.abort_tasks.keys())
                    .chain(self.creation_tasks.keys())
                    .any(|key| &key.runtime == runtime)
        });
    }
}

impl Render for HostManager {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx);
        let palette = cx.global::<Palette>();
        let direction = strings.direction();
        let busy = self.task.is_some() || self.closing;
        let current = self.owner.upgrade().and_then(|owner| {
            owner
                .read(cx)
                .connection
                .as_ref()
                .map(|connection| connection.config.endpoint.clone())
        });
        let rows = self
            .profiles
            .as_ref()
            .map(|profiles| profiles.hosts.clone())
            .unwrap_or_default();
        let default = self
            .profiles
            .as_ref()
            .and_then(|profiles| profiles.default_profile())
            .map(|host| host.id.clone());
        let error = self.error.map(|key| strings.get(key)).unwrap_or_default();
        let status = div()
            .id("host-error")
            .accessibility_id("host-error")
            .role(Role::Status)
            .aria_label(error.clone())
            .a11y_synthetic_children(|builder| {
                builder
                    .parent_node()
                    .set_live(gpui_kit::accesskit::Live::Polite)
            })
            .h(px(24.0))
            .truncate()
            .text_color(palette.error)
            .child(error);
        let (settings_status, picker_status) = if self.presentation == Presentation::Settings {
            (Some(status), None)
        } else {
            (None, Some(status))
        };
        let editor = self.editing.is_some().then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .when(self.presentation == Presentation::Settings, |editor| {
                    editor.w_full().max_w(px(384.0)).when(
                        direction == crate::locale::Direction::RightToLeft,
                        |editor| editor.self_end(),
                    )
                })
                .child(
                    Input::new(&self.label)
                        .id("host-label")
                        .accessibility_id("host-label")
                        .aria_label(strings.get("desktopHostSwitcher.field.labelPlaceholder"))
                        .focus_bordered(false)
                        .when(
                            direction == crate::locale::Direction::RightToLeft,
                            |input| input.text_right(),
                        )
                        .disabled(busy),
                )
                .child(
                    Input::new(&self.url)
                        .id("host-url")
                        .accessibility_id("host-url")
                        .aria_label(strings.get("desktopHostSwitcher.field.urlPlaceholder"))
                        .focus_bordered(false)
                        .text_left()
                        .disabled(busy),
                )
                .child(
                    div()
                        .flex_row_from(direction)
                        .gap(px(8.0))
                        .child(host_button(
                            "host-save".into(),
                            strings.get("desktopHostSwitcher.actions.save"),
                            !busy,
                            palette,
                            cx,
                            |this, _, cx| this.save(cx),
                        ))
                        .child(host_button(
                            "host-cancel".into(),
                            strings.get("desktopHostSwitcher.actions.cancel"),
                            !busy,
                            palette,
                            cx,
                            |this, _, cx| {
                                this.editing = None;
                                this.error = None;
                                cx.notify();
                            },
                        )),
                )
        });
        let (settings_editor, picker_editor) = if self.presentation == Presentation::Settings {
            (editor, None)
        } else {
            (None, editor)
        };
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .text_color(palette.foreground)
            .child(strings.get(if self.presentation == Presentation::Settings {
                "settings.remoteInstances.direct.title"
            } else {
                "desktopHostSwitcher.description"
            }))
            .child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .justify_between()
                    .child(
                        current
                            .as_ref()
                            .map(|endpoint| endpoint.as_str().to_owned())
                            .unwrap_or_default(),
                    )
                    .child(host_button(
                        "host-refresh".into(),
                        strings.get("desktopHostSwitcher.actions.refresh"),
                        !busy,
                        palette,
                        cx,
                        |this, _, cx| this.load(cx),
                    )),
            )
            .children(settings_editor)
            .children(settings_status)
            .children(rows.into_iter().map(|profile| {
                let connect = profile.clone();
                let edit = profile.clone();
                let preferred = profile.id.clone();
                let delete = profile.id.clone();
                let is_default = default.as_deref() == Some(&profile.id);
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        host_button(
                            format!("host-connect:{}", profile.id).into(),
                            profile.label.clone().into(),
                            !busy,
                            palette,
                            cx,
                            move |this, window, cx| this.connect(&connect, window, cx),
                        )
                        .max_w(px(128.0))
                        .min_w_0()
                        .truncate(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(profile.endpoint.as_str().to_owned()),
                    )
                    .child(host_icon_button(
                        format!("host-default:{}", profile.id).into(),
                        if is_default { "star-fill" } else { "star" },
                        strings.get(if is_default {
                            "desktopHostSwitcher.header.default"
                        } else {
                            "desktopHostSwitcher.actions.setAsDefault"
                        }),
                        !busy,
                        palette,
                        cx,
                        move |this, _, cx| {
                            if let Some(mut next) = this.profiles.clone() {
                                let _ = next.set_default(Some(&preferred));
                                this.write(next, false, cx);
                            }
                        },
                    ))
                    .child(host_icon_button(
                        format!("host-edit:{}", profile.id).into(),
                        "edit",
                        strings.get("desktopHostSwitcher.actions.edit"),
                        !busy,
                        palette,
                        cx,
                        move |this, window, cx| this.edit(Some(&edit), window, cx),
                    ))
                    .child(host_icon_button(
                        format!("host-delete:{}", profile.id).into(),
                        "delete-bin",
                        strings.get("desktopHostSwitcher.actions.delete"),
                        !busy,
                        palette,
                        cx,
                        move |this, _, cx| this.remove_profile(&delete, cx),
                    ))
            }))
            .children(picker_editor)
            .when(self.editing.is_none(), |panel| {
                panel.child(host_button(
                    "host-add".into(),
                    strings.get("desktopHostSwitcher.actions.addInstance"),
                    !busy && self.profiles.is_some(),
                    palette,
                    cx,
                    |this, window, cx| this.add_in_settings(window, cx),
                ))
            })
            .children(picker_status)
    }
}

fn host_button(
    id: SharedString,
    label: SharedString,
    enabled: bool,
    palette: &Palette,
    cx: &Context<HostManager>,
    action: impl Fn(&mut HostManager, &mut Window, &mut Context<HostManager>) + 'static,
) -> gpui_kit::base::Button {
    let button = gpui_kit::base::Button::new(id.clone())
        .accessibility_id(id)
        .accessibility_label(label.clone())
        .disabled(!enabled)
        .h(px(28.0))
        .px(px(8.0))
        .rounded(px(7.0))
        .whitespace_nowrap()
        .css_text(0.8125, 1.0, cx)
        .text_color(palette.foreground)
        .child(label);
    on_activate(button, cx, move |this, window, cx| {
        if enabled && this.task.is_none() && !this.closing {
            action(this, window, cx);
        }
    })
}

fn host_icon_button(
    id: SharedString,
    icon: &'static str,
    label: SharedString,
    enabled: bool,
    palette: &Palette,
    cx: &Context<HostManager>,
    action: impl Fn(&mut HostManager, &mut Window, &mut Context<HostManager>) + 'static,
) -> impl IntoElement {
    let button = gpui_kit::base::Button::new(id.clone())
        .accessibility_id(id)
        .accessibility_label(label)
        .disabled(!enabled)
        .size(px(28.0))
        .flex_shrink_0()
        .rounded(px(7.0))
        .child(crate::chrome::glyph(icon, 16.0, palette.foreground));
    on_activate(button, cx, move |this, window, cx| {
        if enabled && this.task.is_none() && !this.closing {
            action(this, window, cx);
        }
    })
}

fn save_error_key(error: ProfilesError) -> &'static str {
    match error {
        ProfilesError::InvalidLabel => "desktopHostSwitcher.error.invalidLabel",
        ProfilesError::InvalidEndpoint => "desktopHostSwitcher.error.invalidUrl",
        ProfilesError::Duplicate => "desktopHostSwitcher.error.duplicateUrl",
        ProfilesError::Limit => "desktopHostSwitcher.error.profileLimit",
        ProfilesError::Changed => "desktopHostSwitcher.error.profilesChanged",
        ProfilesError::Locked => "desktopHostSwitcher.error.profilesBusy",
        ProfilesError::Invalid => "desktopHostSwitcher.error.invalidFile",
        ProfilesError::Unavailable => "desktopHostSwitcher.error.failedToSave",
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext, TestAppContext};
    use openchamber_native_core::ids::SessionId;
    use openchamber_native_transport::endpoint::Endpoint;

    fn close_with_profile_write(cx: &mut TestAppContext, fail: bool) {
        use std::time::{Duration, Instant};
        let fixture = crate::recovery_tests::Fixture::new();
        let store =
            super::ProfileStore::new(fixture.journal().parent().expect("profile directory"));
        let checked_store = store.clone();
        let (window, workspace) = crate::workspace_tests::open(cx);
        let (release, waiting) = tokio::sync::oneshot::channel();
        let mut manager = None;
        cx.update_window(window, |_, window, cx| {
            let host_manager =
                cx.new(|cx| super::HostManager::new(workspace.downgrade(), window, cx));
            host_manager.update(cx, |host, cx| {
                host.pending_write = true;
                host.task = Some(cx.spawn(async move |this, cx| {
                    waiting.await.expect("test releases write");
                    let mut profiles = super::HostProfiles::default();
                    profiles
                        .upsert("host_saved", "Saved", "https://example.test/")
                        .expect("profile");
                    if !fail {
                        store
                            .save(&super::HostProfiles::default(), &profiles)
                            .expect("saved before close");
                    }
                    let _ = this.update(cx, |this, cx| {
                        this.pending_write = false;
                        this.error = fail.then_some("desktopHostSwitcher.error.failedToSave");
                        cx.notify();
                    });
                }));
            });
            workspace.update(cx, |workspace, cx| {
                workspace.host_manager = Some(host_manager.clone());
                workspace.request_close(window, cx);
            });
            manager = Some(host_manager);
        })
        .expect("window");
        assert!(
            cx.update_window(window, |_, _, _| ()).is_ok(),
            "close waits for the profile write"
        );
        release.send(()).expect("release");
        let started = Instant::now();
        loop {
            cx.run_until_parked();
            if fail {
                if cx.read(|cx| workspace.read(cx).host_close.is_none()) {
                    break;
                }
            } else if cx.update_window(window, |_, _, _| ()).is_err() {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(3), "close settles");
            std::thread::sleep(Duration::from_millis(1));
        }
        if fail {
            assert!(
                cx.update_window(window, |_, _, _| ()).is_ok(),
                "a failed write leaves the window open"
            );
            assert!(!cx.read(|cx| manager.as_ref().expect("manager").read(cx).closing));
        } else {
            assert_eq!(checked_store.load().expect("saved file").hosts.len(), 1);
        }
    }

    #[gpui_kit::test]
    fn close_drains_a_pending_profile_write(cx: &mut TestAppContext) {
        close_with_profile_write(cx, false);
    }

    #[gpui_kit::test]
    fn failed_profile_write_keeps_the_window_open(cx: &mut TestAppContext) {
        close_with_profile_write(cx, true);
    }

    #[gpui_kit::test]
    fn a_host_switch_clears_the_previous_password_and_trust_choice(cx: &mut TestAppContext) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        let fixture = crate::recovery_tests::Fixture::new();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_persistence(fixture.journal(), window, cx);
                workspace
                    .authentication
                    .password
                    .update(cx, |input, cx| input.set_value("host-a-only", window, cx));
                workspace.authentication.trust_device = true;
                workspace.authentication.visible = true;
                workspace.connect_to_host(
                    Endpoint::parse("http://127.0.0.1:1/").expect("endpoint"),
                    window,
                    cx,
                );
                assert!(
                    workspace
                        .authentication
                        .password
                        .read(cx)
                        .value()
                        .is_empty()
                );
                assert!(!workspace.authentication.trust_device);
                assert!(!workspace.authentication.submitting);
                assert!(
                    workspace
                        .connection
                        .as_ref()
                        .expect("connection")
                        .config
                        .password
                        .is_none()
                );
            });
        })
        .expect("window");
    }

    #[gpui_kit::test]
    fn switching_keeps_the_captured_mutation_executor_until_its_chain_finishes(
        cx: &mut TestAppContext,
    ) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::{Duration, Instant};
        let (window, workspace) = crate::workspace_tests::open(cx);
        let fixture = crate::recovery_tests::Fixture::new();
        let (release, wait) = tokio::sync::oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let observed = finished.clone();
        let mut key = None;
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_persistence(fixture.journal(), window, cx);
                let mut old = crate::connection::Connection::new(
                    crate::connection::ServerConfig::for_endpoint(
                        Endpoint::parse("http://127.0.0.1:2/").expect("endpoint"),
                    ),
                )
                .expect("network");
                old.generation = 10;
                let task = old.network.run(async move {
                    if wait.await.is_ok() {
                        observed.store(true, Ordering::Release);
                    }
                });
                let pending = crate::send_attempt::SessionKey {
                    runtime: workspace.runtime.clone(),
                    session: SessionId::from("ses_host_switch"),
                };
                key = Some(pending.clone());
                workspace.mutation_tasks.insert(
                    pending,
                    cx.spawn(async move |_, _| {
                        task.await;
                    }),
                );
                workspace.connection = Some(old);
                workspace.connect_to_host(
                    Endpoint::parse("http://127.0.0.1:1/").expect("endpoint"),
                    window,
                    cx,
                );
                assert_eq!(workspace.retired_networks.len(), 1);
                assert!(
                    workspace
                        .connection
                        .as_ref()
                        .expect("new connection")
                        .generation
                        > 10
                );
            });
        })
        .expect("window");
        release.send(()).expect("old executor still alive");
        let started = Instant::now();
        while !finished.load(Ordering::Acquire) {
            cx.run_until_parked();
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "retained mutation must finish"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        cx.update(|cx| {
            workspace.update(cx, |workspace, _| {
                workspace.mutation_tasks.remove(&key.expect("key"));
                workspace.release_retired_networks();
                assert!(workspace.retired_networks.is_empty());
            })
        });
    }
}
