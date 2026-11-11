//! Settings → About (`AboutSettings.tsx`, desktop layout): the brand with its
//! links, then one run of version rows. The native app talks to a server it
//! does not ship, so it shows its own version, the server's OpenChamber
//! version and the server's OpenCode version with the OpenCode update the
//! server can install itself.
//!
//! An install outlasts a request deadline and the server finishes it whether
//! or not this window still waits, so it is tracked per runtime outside the
//! Settings close barrier: one request at a time, kept across a reconnect to
//! the same runtime. Only the server's own answer counts as a failure; a lost
//! answer is unknown until the status is read again.

use std::collections::HashMap;

use gpui_kit::component::Theme;
use gpui_kit::{
    AnyElement, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Render, Role, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Task, TestSupportExt, WeakEntity, Window, div, px, svg,
};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_transport::opencode_upgrade::{OpenCodeUpgradeError, OpenCodeUpgradeStatus};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::network::{Network, NetworkTask};
use crate::settings_controls::{
    Load, ServerTarget, field_row, helper, load_failed, page_frame, same_target,
};
use crate::settings_navigation::Page;
use crate::theme::Palette;
use crate::workspace::Workspace;

const LINKS: [(&str, &str, &str, &str); 3] = [
    (
        "about-settings-github",
        "github-fill",
        "GitHub",
        "https://github.com/openchamber/openchamber",
    ),
    (
        "about-settings-discord",
        "discord-fill",
        "Discord",
        "https://discord.gg/ZYRSdnwwKA",
    ),
    (
        "about-settings-social",
        "twitter-xfill",
        "@openchamber_dev",
        "https://x.com/openchamber_dev",
    ),
];

/// What this page knows about an upgrade it started on the shown runtime.
/// Whether one is still running is `outstanding`, not a phase.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Installed; OpenCode runs it after a reload.
    Installed(Option<String>),
    /// The server said the install failed, with its reason. The button stays
    /// to try again.
    Failed(SharedString),
    /// The request ended without the server's answer. Nothing is claimed and
    /// nothing can be started until a status read succeeds.
    Unknown,
}

pub(crate) struct AboutPage {
    owner: WeakEntity<Workspace>,
    network: Option<Network>,
    target: Option<ServerTarget>,
    /// Whether Settings shows this page. A hidden page reads nothing.
    shown: bool,
    /// The server's OpenChamber version.
    server: Load<String>,
    /// OpenCode on the server. A failed reread keeps the last status, so a
    /// failed check never turns into "no update".
    opencode: Load<OpenCodeUpgradeStatus>,
    phase: Phase,
    /// The status read that should settle `Phase::Unknown` failed.
    status_read_failed: bool,
    /// The install request in flight per runtime. A runtime with an entry
    /// accepts no second one.
    outstanding: HashMap<RuntimeId, Task<()>>,
    read_revision: u64,
    read: Option<Task<()>>,
    scroll: ScrollHandle,
    links: [FocusHandle; 3],
    _subscriptions: Vec<Subscription>,
}

impl AboutPage {
    pub(crate) fn new(owner: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(workspace) = owner.upgrade() {
            subscriptions.push(cx.observe(&workspace, |this, _, cx| {
                if !this.shown {
                    return;
                }
                let target = ServerTarget::capture(&this.owner, this.network.as_ref(), cx);
                if !same_target(this.target.as_ref(), target.as_ref()) {
                    this.refresh(cx);
                }
            }));
        }
        Self {
            owner,
            network: Network::new().ok(),
            target: None,
            shown: false,
            server: Load::Loading,
            opencode: Load::Loading,
            phase: Phase::Idle,
            status_read_failed: false,
            outstanding: HashMap::new(),
            read_revision: 0,
            read: None,
            scroll: ScrollHandle::new(),
            links: std::array::from_fn(|_| cx.focus_handle().tab_index(0).tab_stop(true)),
            _subscriptions: subscriptions,
        }
    }

    /// Settings shows or leaves this page. Showing it reads again.
    pub(crate) fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.shown = shown;
        if shown {
            self.refresh(cx);
        }
    }

    fn runtime(&self) -> Option<&RuntimeId> {
        self.target.as_ref().map(|target| &target.runtime)
    }

    fn upgrading(&self) -> bool {
        self.runtime()
            .is_some_and(|runtime| self.outstanding.contains_key(runtime))
    }

    /// Reads the server's version and the upgrade status, one request each.
    /// The reads are independent: one failing leaves the other row complete.
    /// A reconnect to the same runtime keeps what is shown, the phase
    /// included; another runtime starts over.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let target = ServerTarget::capture(&self.owner, self.network.as_ref(), cx);
        if self.runtime() != target.as_ref().map(|target| &target.runtime) {
            // Another server's versions and upgrade outcome never carry over.
            self.server = Load::Loading;
            self.opencode = Load::Loading;
            self.phase = Phase::Idle;
            self.status_read_failed = false;
        }
        self.target = target.clone();
        let Some(target) = target else {
            self.read = None;
            cx.notify();
            return;
        };
        if self.server == Load::Failed {
            self.server = Load::Loading;
        }
        if self.opencode == Load::Failed {
            self.opencode = Load::Loading;
        }
        self.read_revision += 1;
        let revision = self.read_revision;
        let client = target.client;
        let runtime = target.runtime;
        let reading = NetworkTask::spawn_on(&target.network, async move {
            tokio::join!(
                client.openchamber_version(),
                client.opencode_upgrade_status()
            )
        });
        self.read = Some(cx.spawn(async move |this, cx| {
            let result = reading.await;
            let _ = this.update(cx, |this, cx| {
                if this.read_revision != revision || this.runtime() != Some(&runtime) {
                    return;
                }
                this.read = None;
                let (server, opencode) = match result {
                    Some((server, opencode)) => (server.ok(), opencode.ok()),
                    None => (None, None),
                };
                match server {
                    Some(version) => this.server = Load::Ready(version),
                    None if this.server.ready().is_some() => {}
                    None => this.server = Load::Failed,
                }
                this.status_read_failed = opencode.is_none();
                match opencode {
                    Some(status) => {
                        match &this.phase {
                            // The status is the authority once it is read:
                            // it shows whether the unanswered install landed.
                            Phase::Unknown => this.phase = Phase::Idle,
                            // Once the server runs the installed version,
                            // the status replaces the stand-in note.
                            Phase::Installed(installed) => {
                                let running = status.current_version.as_deref();
                                let same = installed.as_deref().is_none_or(|version| {
                                    Some(version.trim_start_matches('v')) == running
                                });
                                if running.is_some() && same && status.available_version.is_none() {
                                    this.phase = Phase::Idle;
                                }
                            }
                            Phase::Idle | Phase::Failed(_) => {}
                        }
                        this.opencode = Load::Ready(status);
                    }
                    None if this.opencode.ready().is_some() => {}
                    None => this.opencode = Load::Failed,
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The version the OpenCode row offers to install.
    fn offered(&self) -> Option<(&str, bool)> {
        if matches!(self.phase, Phase::Installed(_)) {
            return None;
        }
        let status = self.opencode.ready()?;
        Some((status.available_version.as_deref()?, status.supported))
    }

    /// Starts the install on the shown runtime unless one is outstanding
    /// there or the last one's outcome is still unknown.
    fn upgrade(&mut self, cx: &mut Context<Self>) {
        if self.upgrading() || self.phase == Phase::Unknown {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        if !matches!(self.offered(), Some((_, true))) {
            return;
        }
        self.phase = Phase::Idle;
        let client = target.client.clone();
        let installing =
            NetworkTask::spawn_on(
                &target.network,
                async move { client.upgrade_opencode().await },
            );
        let runtime = target.runtime.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = installing.await;
            let _ = this.update(cx, |this, cx| {
                this.outstanding.remove(&runtime);
                if this.runtime() != Some(&runtime) {
                    // Another runtime is shown; its own status read decides
                    // what this one shows when it comes back.
                    return;
                }
                match result {
                    Some(Ok(version)) => this.phase = Phase::Installed(version),
                    Some(Err(OpenCodeUpgradeError::Failed { message })) => {
                        this.phase = Phase::Failed(
                            message.filter(|message| !message.is_empty()).map_or_else(
                                || {
                                    locale::strings(cx)
                                        .get("opencodeUpdate.toast.failed.description")
                                },
                                SharedString::from,
                            ),
                        );
                    }
                    // No answer: the install may be running or done.
                    _ => {
                        this.phase = Phase::Unknown;
                        if this.shown {
                            this.refresh(cx);
                        } else {
                            this.status_read_failed = true;
                        }
                    }
                }
                cx.notify();
            });
        });
        self.outstanding.insert(target.runtime, task);
        cx.notify();
    }

    fn link(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let (id, icon, label, url) = LINKS[index];
        let palette = cx.global::<Palette>().clone();
        let foreground = palette.foreground;
        on_activate(
            div()
                .id(id)
                .test_support()
                .accessibility_id(id)
                .aria_label(label)
                .role(Role::Link)
                .track_focus(&self.links[index])
                .tab_index(0)
                .flex_row_from(locale::strings(cx).direction())
                .items_center()
                .gap(px(6.0))
                .css_text(0.8125, 1.25, cx)
                .text_color(palette.muted_foreground)
                .cursor_pointer()
                .hover(move |style| style.text_color(foreground))
                .child(glyph(icon, 16.0, palette.muted_foreground))
                .child(label),
            cx,
            move |_, window, cx| crate::markdown_links::open_user_link(url, window, cx),
        )
        .into_any_element()
    }

    fn version_value(
        &self,
        id: &'static str,
        text: SharedString,
        cx: &Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .accessibility_id(id)
            .role(Role::Paragraph)
            .aria_label(text.clone())
            .css_text(0.8125, 1.25, cx)
            .font_family(Theme::global(cx).mono_font_family.clone())
            .text_color(cx.global::<Palette>().muted_foreground)
            .child(text)
            .into_any_element()
    }
}

impl Render for AboutPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let unknown = strings.get("settings.openchamber.about.state.unknown");
        let loading = strings.get("common.loading");
        let links: Vec<AnyElement> = (0..LINKS.len()).map(|index| self.link(index, cx)).collect();
        let brand = div()
            .flex_row_from(direction)
            .items_center()
            .gap(px(16.0))
            .child(
                div()
                    .id("about-settings-logo")
                    .role(Role::Image)
                    .aria_label(strings.get("openChamberLogo.aria.logo"))
                    .size(px(48.0))
                    .flex_shrink_0()
                    .child(
                        svg()
                            .path(crate::assets::APP_LOGO)
                            .size(px(48.0))
                            .text_color(palette.foreground),
                    ),
            )
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .id("about-title")
                            .role(Role::Heading)
                            .css_text(1.125, 1.75, cx)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("OpenChamber"),
                    )
                    .child(
                        div()
                            .flex_row_from(direction)
                            .flex_wrap()
                            .items_center()
                            .gap_x(px(16.0))
                            .gap_y(px(4.0))
                            .children(links),
                    ),
            );

        let server_text = match (&self.target, &self.server) {
            (None, _) | (_, Load::Failed) => unknown.clone(),
            (_, Load::Loading) => loading.clone(),
            (_, Load::Ready(version)) => version.clone().into(),
        };
        let opencode_text = match (&self.target, &self.opencode) {
            (None, _) | (_, Load::Failed) => unknown.clone(),
            (_, Load::Loading) => loading,
            (_, Load::Ready(status)) => status
                .current_version
                .clone()
                .map_or(unknown, SharedString::from),
        };
        let read_failed = self.target.is_some()
            && (self.server == Load::Failed
                || self.opencode == Load::Failed
                || (self.phase == Phase::Unknown && self.status_read_failed));

        // The OpenCode update as a button in its row and a line under the rows.
        let offered = self
            .offered()
            .map(|(version, supported)| (version.to_owned(), supported));
        let upgrading = self.upgrading();
        let unknown_outcome = self.phase == Phase::Unknown;
        let action = match &offered {
            Some((version, true)) => {
                let label = if upgrading {
                    strings.get("opencodeUpdate.toast.upgrading.title")
                } else {
                    strings.format(
                        "settings.openchamber.about.actions.updateOpenCodeToVersion",
                        &[("version", version)],
                    )
                };
                Some(
                    on_activate(
                        gpui_kit::base::Button::new("about-settings-upgrade-opencode")
                            .accessibility_id("about-settings-upgrade-opencode")
                            .accessibility_label(label.clone())
                            .disabled(upgrading || unknown_outcome)
                            .h(gpui_kit::rems(1.75))
                            .px(px(8.0))
                            .rounded(px(7.0))
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(6.0))
                            .child(glyph("download", 16.0, palette.foreground))
                            .child(label),
                        cx,
                        |this, _, cx| this.upgrade(cx),
                    )
                    .into_any_element(),
                )
            }
            _ => None,
        };
        let note: Option<AnyElement> = match (&self.phase, &offered) {
            (Phase::Installed(Some(version)), _) => Some(helper(
                strings.format(
                    "settings.openchamber.about.openCode.installedVersion",
                    &[("version", version)],
                ),
                cx,
            )),
            (Phase::Installed(None), _) => Some(helper(
                strings.get("settings.openchamber.about.openCode.installed"),
                cx,
            )),
            (Phase::Failed(reason), _) => Some(
                div()
                    .id("about-settings-upgrade-failed")
                    .accessibility_id("about-settings-upgrade-failed")
                    .role(Role::Status)
                    .aria_label(reason.clone())
                    .css_text(0.8125, 1.25, cx)
                    .text_color(palette.status_error)
                    .child(reason.clone())
                    .into_any_element(),
            ),
            (Phase::Unknown, _) if !self.status_read_failed => Some(helper(
                strings.get("settings.openchamber.about.state.checking"),
                cx,
            )),
            (_, Some((version, false))) => Some(helper(
                strings.format(
                    "settings.openchamber.about.openCode.manualUpdate",
                    &[("version", version)],
                ),
                cx,
            )),
            _ => None,
        };

        let app = self.version_value(
            "about-settings-app-version",
            env!("CARGO_PKG_VERSION").into(),
            cx,
        );
        let server = self.version_value("about-settings-server-version", server_text, cx);
        let opencode = div()
            .flex_row_from(direction)
            .flex_wrap()
            .items_center()
            .gap(px(8.0))
            .child(self.version_value("about-settings-opencode-version", opencode_text, cx))
            .children(action)
            .into_any_element();
        let failed = read_failed.then(|| {
            load_failed(
                "about-settings-load-failed",
                strings.get("settings.openchamber.desktopNetwork.error.loadFailed"),
                strings.get("desktopHostSwitcher.actions.retry"),
                cx,
                |this, cx| this.refresh(cx),
            )
        });
        page_frame("about-settings-page", Page::About, &self.scroll, cx)
            .child(brand)
            .child(
                div()
                    .mt(px(32.0))
                    .pt(px(24.0))
                    .border_t_1()
                    .border_color(palette.border)
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(field_row(
                        "about.app-version",
                        strings.get("settings.openchamber.about.field.appVersion"),
                        None,
                        app,
                        None,
                        cx,
                    ))
                    .child(field_row(
                        "about.server-version",
                        strings.get("settings.openchamber.about.field.serverVersion"),
                        None,
                        server,
                        None,
                        cx,
                    ))
                    .child(field_row(
                        "about.opencode-version",
                        strings.get("settings.openchamber.about.field.openCodeVersion"),
                        None,
                        opencode,
                        None,
                        cx,
                    ))
                    .children(failed)
                    .children(note.map(|note| div().pt(px(8.0)).child(note))),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext};
    use serde_json::json;

    use super::*;
    use crate::settings_controls::tests::{ResponseGate, retarget};
    use crate::settings_test_server::{Server, open, settings, settle};

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Install {
        Succeeds,
        /// The server answers that the install failed.
        Refused,
        /// No usable answer: a redirect, which the client never follows.
        Unanswered,
    }

    struct Fixture {
        server: Server,
        fail_reads: Arc<AtomicBool>,
        refuse: Arc<AtomicBool>,
    }

    /// A server on `version` whose OpenCode 2.2.0 has 2.3.0 available until
    /// an install lands. `gate` holds every install request.
    fn fixture(
        version: &'static str,
        install: Install,
        gate: Option<Arc<ResponseGate>>,
    ) -> Fixture {
        let fail_reads = Arc::new(AtomicBool::new(false));
        let refuse = Arc::new(AtomicBool::new(install == Install::Refused));
        let installed = Arc::new(AtomicBool::new(false));
        let (reads, refused) = (Arc::clone(&fail_reads), Arc::clone(&refuse));
        let server = Server::new(move |request| {
            if request.method == "GET" && reads.load(Ordering::Acquire) {
                return (500, json!({"error": "fixture read failure"}));
            }
            if request.is("GET", "/api/system/info") {
                (200, json!({"openchamberVersion": version}))
            } else if request.is("GET", "/api/opencode/upgrade-status") {
                let done = installed.load(Ordering::Acquire);
                (
                    200,
                    json!({
                        "available": !done,
                        "currentVersion": if done { "2.3.0" } else { "2.2.0" },
                        "latestVersion": "2.3.0",
                        "upgrade": {"supported": true},
                    }),
                )
            } else if request.is("POST", "/api/opencode/upgrade") {
                if let Some(gate) = &gate {
                    gate.hold();
                }
                if refused.load(Ordering::Acquire) {
                    return (500, json!({"error": "npm is missing"}));
                }
                installed.store(true, Ordering::Release);
                if install == Install::Unanswered {
                    (302, json!({}))
                } else {
                    (200, json!({"success": true, "version": "2.3.0"}))
                }
            } else {
                (404, json!({"error": "not in this fixture"}))
            }
        });
        Fixture {
            server,
            fail_reads,
            refuse,
        }
    }

    fn page(workspace: &Entity<Workspace>, cx: &TestAppContext) -> Entity<AboutPage> {
        let settings = settings(workspace, cx);
        cx.read(|cx| settings.read(cx).about.clone())
    }

    fn act(
        window: AnyWindowHandle,
        page: &Entity<AboutPage>,
        cx: &mut TestAppContext,
        action: impl FnOnce(&mut AboutPage, &mut Context<AboutPage>),
    ) {
        cx.update_window(window, |_, _, cx| {
            page.update(cx, |page, cx| action(page, cx));
        })
        .expect("test window");
        settle(window, cx);
    }

    fn installs(fixture: &Fixture) -> usize {
        fixture.server.writes().len()
    }

    fn status_reads(server: &Server) -> usize {
        server
            .requests()
            .iter()
            .filter(|request| request.is("GET", "/api/opencode/upgrade-status"))
            .count()
    }

    #[gpui_kit::test]
    fn both_versions_load_with_one_status_request_per_refresh(cx: &mut TestAppContext) {
        let fixture = fixture("2.6.1", Install::Succeeds, None);
        let (window, workspace) = open(&fixture.server, Page::About, cx);
        let page = page(&workspace, cx);
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.server, Load::Ready("2.6.1".into()));
            assert_eq!(page.offered(), Some(("2.3.0", true)));
        });
        let before = status_reads(&fixture.server);
        act(window, &page, cx, |page, cx| page.refresh(cx));
        assert_eq!(
            status_reads(&fixture.server) - before,
            1,
            "a refresh asks for the upgrade status once"
        );
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("about-settings-upgrade-opencode").is_some());
            assert!(window.try_find("about-settings-load-failed").is_none());
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn a_failed_read_offers_retry_and_no_update_claim(cx: &mut TestAppContext) {
        let fixture = fixture("2.6.1", Install::Succeeds, None);
        fixture.fail_reads.store(true, Ordering::Release);
        let (window, workspace) = open(&fixture.server, Page::About, cx);
        let page = page(&workspace, cx);
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.server, Load::Failed);
            assert_eq!(page.opencode, Load::Failed);
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window
                    .try_find("about-settings-load-failed-retry")
                    .is_some()
            );
            assert!(window.try_find("about-settings-upgrade-opencode").is_none());
        })
        .expect("test window");
        fixture.fail_reads.store(false, Ordering::Release);
        act(window, &page, cx, |page, cx| page.refresh(cx));
        cx.read(|cx| assert_eq!(page.read(cx).server, Load::Ready("2.6.1".into())));
    }

    #[gpui_kit::test]
    fn the_servers_refusal_is_a_failure_and_the_same_button_installs(cx: &mut TestAppContext) {
        let fixture = fixture("2.6.1", Install::Refused, None);
        let (window, workspace) = open(&fixture.server, Page::About, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, cx| page.upgrade(cx));
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.phase, Phase::Failed("npm is missing".into()));
            assert_eq!(page.offered(), Some(("2.3.0", true)));
            assert!(!page.upgrading());
        });
        fixture.refuse.store(false, Ordering::Release);
        act(window, &page, cx, |page, cx| page.upgrade(cx));
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.phase, Phase::Installed(Some("2.3.0".into())));
            assert_eq!(page.offered(), None, "an installed update is not offered");
        });
        assert_eq!(installs(&fixture), 2);
    }

    #[gpui_kit::test]
    fn a_lost_answer_is_unknown_and_the_status_read_decides(cx: &mut TestAppContext) {
        let fixture = fixture("2.6.1", Install::Unanswered, None);
        let (window, workspace) = open(&fixture.server, Page::About, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, cx| page.upgrade(cx));
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(
                page.phase,
                Phase::Idle,
                "the reread status settles the unknown outcome"
            );
            assert_eq!(
                page.offered(),
                None,
                "the server runs 2.3.0, so nothing is offered again"
            );
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find("about-settings-upgrade-failed").is_none(),
                "a lost answer is never shown as a failed install"
            );
        })
        .expect("test window");
        assert_eq!(installs(&fixture), 1);
    }

    #[gpui_kit::test]
    fn an_unknown_outcome_blocks_another_install_until_a_status_read_succeeds(
        cx: &mut TestAppContext,
    ) {
        let fixture = fixture("2.6.1", Install::Unanswered, None);
        let (window, workspace) = open(&fixture.server, Page::About, cx);
        let page = page(&workspace, cx);
        cx.update_window(window, |_, _, cx| {
            page.update(cx, |page, cx| page.upgrade(cx));
        })
        .expect("test window");
        // The status read that follows the lost answer fails too.
        fixture.fail_reads.store(true, Ordering::Release);
        settle(window, cx);
        act(window, &page, cx, |page, cx| page.upgrade(cx));
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.phase, Phase::Unknown);
            assert!(page.status_read_failed);
        });
        assert_eq!(
            installs(&fixture),
            1,
            "no second install on an unknown outcome"
        );
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window
                    .try_find("about-settings-load-failed-retry")
                    .is_some()
            );
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn one_install_at_a_time_survives_a_reconnect_to_the_same_runtime(cx: &mut TestAppContext) {
        let gate = ResponseGate::new();
        let fixture = fixture("2.6.1", Install::Succeeds, Some(Arc::clone(&gate)));
        let (window, workspace) = open(&fixture.server, Page::About, cx);
        let page = page(&workspace, cx);
        cx.update_window(window, |_, _, cx| {
            page.update(cx, |page, cx| {
                page.upgrade(cx);
                // A second press while the first request is outstanding.
                page.upgrade(cx);
            });
        })
        .expect("test window");
        gate.entered(cx);
        // The connection is replaced; the runtime is the same server.
        workspace.update(cx, |workspace, cx| {
            workspace
                .connection
                .as_mut()
                .expect("connection")
                .generation += 1;
            cx.notify();
        });
        cx.run_until_parked();
        cx.update_window(window, |_, _, cx| {
            page.update(cx, |page, cx| {
                assert!(page.upgrading(), "the install is still shown as running");
                page.upgrade(cx);
            });
        })
        .expect("test window");
        gate.release();
        settle(window, cx);
        cx.read(|cx| {
            let page = page.read(cx);
            assert!(!page.upgrading(), "the answer lands after the reconnect");
            // The reconnect's own status read is answered behind the held
            // install. When it lands last it already reports the installed
            // version, which replaces the stand-in note.
            let running = page
                .opencode
                .ready()
                .and_then(|status| status.current_version.as_deref());
            assert!(
                page.phase == Phase::Installed(Some("2.3.0".into()))
                    || (page.phase == Phase::Idle && running == Some("2.3.0")),
                "the install is shown as done: {:?}, running {running:?}",
                page.phase
            );
        });
        assert_eq!(installs(&fixture), 1, "one request for three presses");
    }

    #[gpui_kit::test]
    fn an_install_is_outside_the_close_barrier(cx: &mut TestAppContext) {
        let gate = ResponseGate::new();
        let fixture = fixture("2.6.1", Install::Succeeds, Some(Arc::clone(&gate)));
        let (window, workspace) = open(&fixture.server, Page::About, cx);
        let page = page(&workspace, cx);
        cx.update_window(window, |_, _, cx| {
            page.update(cx, |page, cx| page.upgrade(cx));
        })
        .expect("test window");
        gate.entered(cx);
        let settings = settings(&workspace, cx);
        let pending = settings.update(cx, |settings, cx| settings.take_pending_write(cx));
        assert!(
            pending.is_none(),
            "closing Settings does not wait for the server's install"
        );
        settings.update(cx, |settings, cx| settings.cancel_close(cx));
        gate.release();
        settle(window, cx);
    }

    #[gpui_kit::test]
    fn a_runtime_switch_drops_the_retired_servers_versions(cx: &mut TestAppContext) {
        let gate = ResponseGate::new();
        let held = Arc::clone(&gate);
        let held_once = Arc::new(AtomicBool::new(false));
        let first = Server::new(move |request| {
            if request.is("GET", "/api/system/info") && !held_once.swap(true, Ordering::AcqRel) {
                held.hold();
            }
            (200, json!({"openchamberVersion": "1.0.0-retired"}))
        });
        let second = fixture("2.6.1", Install::Succeeds, None);
        let (window, workspace) = open(&first, Page::About, cx);
        let page = page(&workspace, cx);
        // The first server still holds the page's read while the runtime changes.
        retarget(&workspace, &second.server, cx);
        settle(window, cx);
        gate.release();
        settle(window, cx);
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).server,
                Load::Ready("2.6.1".into()),
                "the retired server's answer never shows for the new one"
            );
        });
    }

    #[gpui_kit::test]
    fn a_page_that_is_not_the_visible_one_reads_nothing(cx: &mut TestAppContext) {
        let fixture = fixture("2.6.1", Install::Succeeds, None);
        let (window, workspace) = open(&fixture.server, Page::General, cx);
        let page = page(&workspace, cx);
        assert_eq!(status_reads(&fixture.server), 0);
        cx.read(|cx| assert_eq!(page.read(cx).read_revision, 0));
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.show_settings(Some(Page::About), window, cx)
            });
        })
        .expect("test window");
        settle(window, cx);
        assert_eq!(
            status_reads(&fixture.server),
            1,
            "visiting the page reads once"
        );
    }

    #[gpui_kit::test]
    fn a_hidden_page_reads_nothing_when_the_connection_changes(cx: &mut TestAppContext) {
        let first = fixture("2.6.1", Install::Succeeds, None);
        let second = fixture("2.7.0", Install::Succeeds, None);
        let (window, workspace) = open(&first.server, Page::About, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, cx| page.set_shown(false, cx));
        retarget(&workspace, &second.server, cx);
        settle(window, cx);
        assert_eq!(status_reads(&second.server), 0);
        act(window, &page, cx, |page, cx| page.set_shown(true, cx));
        assert_eq!(
            status_reads(&second.server),
            1,
            "showing the page reads once"
        );
        cx.read(|cx| assert_eq!(page.read(cx).server, Load::Ready("2.7.0".into())));
    }
}
