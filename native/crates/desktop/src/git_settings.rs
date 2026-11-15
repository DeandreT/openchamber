//! Settings → Git (`GitPage.tsx`, `GitSettings.tsx`): the stored commit
//! identities with the default one, and the Git preferences of the settings
//! document. Every control writes at once. A failed write puts the control
//! back on the value the server last confirmed, for that field alone, and
//! offers Retry for that change.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use gpui_kit::{
    AnyElement, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Render, Role, ScrollAnchor, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, TestSupportExt, Toggled, WeakEntity, Window, div, prelude::FluentBuilder,
    px,
};
use openchamber_native_core::git_identity::Identity;
use openchamber_native_transport::git_settings::{
    ChangesView, GitPreferenceChange, GitPreferences,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::network::{Network, NetworkTask};
use crate::settings_controls::{
    Load, ServerTarget, SettingsWrites, checkbox_row, confirm, field_row, focus_handle,
    load_failed, page_frame, same_target, section_with_info,
};
use crate::settings_navigation::Page;
use crate::theme::Palette;
use crate::workspace::Workspace;

const IDENTITIES: usize = 0;
const CHANGES_VIEW: usize = 1;
const GITMOJI: usize = 2;

/// Everything the page shows, from one round of reads.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Loaded {
    /// The machine's own author; `None` when it has neither name nor email.
    system: Option<Identity>,
    profiles: Vec<Identity>,
    /// What the controls show: the confirmed values with changes in flight.
    preferences: GitPreferences,
    /// The preferences the server last read or answered with. A failed write
    /// restores its own field from here, never from another change in flight.
    confirmed: GitPreferences,
}

/// Copies the one field `change` writes from `from` into `into`.
fn copy_field(change: &GitPreferenceChange, from: &GitPreferences, into: &mut GitPreferences) {
    match change {
        GitPreferenceChange::DefaultIdentity(_) => {
            into.default_identity = from.default_identity.clone();
        }
        GitPreferenceChange::GitmojiEnabled(_) => into.gitmoji_enabled = from.gitmoji_enabled,
        GitPreferenceChange::ChangesView(_) => into.changes_view = from.changes_view,
    }
}

fn same_field(a: &GitPreferenceChange, b: &GitPreferenceChange) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

/// A write the server did not take, kept so Retry can send it again.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FailedWrite {
    Preference(GitPreferenceChange),
    Delete { id: String, name: String },
}

pub(crate) struct GitSettingsPage {
    owner: WeakEntity<Workspace>,
    network: Option<Network>,
    target: Option<ServerTarget>,
    load: Load<Loaded>,
    /// At most one per preference field and one per identity.
    failed: Vec<FailedWrite>,
    /// Identities with a delete accepted and not answered.
    deleting: HashSet<String>,
    /// Whether Settings shows this page. A hidden page reads nothing.
    shown: bool,
    /// Retry was pressed; the next render, which has the window, sends it.
    retry_requested: bool,
    /// Feedback of the last action, kept for tests.
    notice: Option<SharedString>,
    read_revision: u64,
    /// Writes started; a read that began before one is dropped.
    mutations: u64,
    read: Option<Task<()>>,
    writes: SettingsWrites,
    scroll: ScrollHandle,
    anchors: [ScrollAnchor; 3],
    focus: HashMap<SharedString, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl GitSettingsPage {
    pub(crate) fn new(owner: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(workspace) = owner.upgrade() {
            subscriptions.push(cx.observe(&workspace, |this, _, cx| {
                let target = ServerTarget::capture(&this.owner, this.network.as_ref(), cx);
                if same_target(this.target.as_ref(), target.as_ref()) {
                    return;
                }
                if this.shown {
                    this.refresh(cx);
                } else if this.target.is_some() {
                    // Hidden: the old server's state is let go without a
                    // read. Showing the page reads the current one.
                    this.retire();
                    this.target = None;
                    this.read = None;
                    cx.notify();
                }
            }));
        }
        let scroll = ScrollHandle::new();
        Self {
            owner,
            network: Network::new().ok(),
            target: None,
            load: Load::Loading,
            failed: Vec::new(),
            deleting: HashSet::new(),
            shown: false,
            retry_requested: false,
            notice: None,
            read_revision: 0,
            mutations: 0,
            read: None,
            writes: SettingsWrites::default(),
            anchors: std::array::from_fn(|_| ScrollAnchor::for_handle(scroll.clone())),
            scroll,
            focus: HashMap::new(),
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

    fn retire(&mut self) {
        self.load = Load::Loading;
        self.failed.clear();
        self.deleting.clear();
    }

    /// Reads the identities and the preferences for the current server. Any
    /// failed read fails the page visibly: a missing list is never shown as
    /// "no identities", and unread preferences never show as defaults.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let target = ServerTarget::capture(&self.owner, self.network.as_ref(), cx);
        if !same_target(self.target.as_ref(), target.as_ref()) {
            // Another server's identities and failed changes never carry over.
            self.retire();
        }
        self.target = target.clone();
        let Some(target) = target else {
            self.read = None;
            cx.notify();
            return;
        };
        if self.load == Load::Failed {
            self.load = Load::Loading;
        }
        self.read_revision += 1;
        let (revision, mutations) = (self.read_revision, self.mutations);
        let client = target.client;
        let reading = NetworkTask::spawn_on(&target.network, async move {
            let (profiles, system, preferences) = tokio::join!(
                client.git_identities(),
                client.system_identity(),
                client.git_preferences()
            );
            // The machine's author is part of the list: without its read the
            // page cannot say whether it exists.
            let system = system.ok()?;
            let preferences = preferences.ok()?;
            Some(Loaded {
                profiles: profiles.ok()?,
                system: Some(system)
                    .filter(|system| !system.user_name.is_empty() || !system.user_email.is_empty()),
                confirmed: preferences.clone(),
                preferences,
            })
        });
        self.read = Some(cx.spawn(async move |this, cx| {
            let result = reading.await.flatten();
            let _ = this.update(cx, |this, cx| {
                if this.read_revision != revision || this.mutations != mutations {
                    return;
                }
                this.read = None;
                match result {
                    Some(loaded) => {
                        // A delete that failed has nothing left to retry once
                        // its row is gone.
                        this.failed.retain(|failed| match failed {
                            FailedWrite::Delete { id, .. } => {
                                loaded.profiles.iter().any(|profile| &profile.id == id)
                            }
                            FailedWrite::Preference(_) => true,
                        });
                        this.load = Load::Ready(loaded);
                    }
                    // What is shown stays through a failed reread.
                    None if this.load.ready().is_some() => {}
                    None => this.load = Load::Failed,
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn toast(
        &mut self,
        key: &str,
        name: Option<&str>,
        success: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let strings = locale::strings(cx);
        let text = match name {
            Some(name) => strings.format(key, &[("name", name)]),
            None => strings.get(key),
        };
        self.notice = Some(text.clone());
        let notification = if success {
            Notification::success(text)
        } else {
            Notification::error(text)
        };
        window.push_notification(notification, cx);
    }

    fn current(&self, target: &ServerTarget, cx: &gpui_kit::App) -> bool {
        same_target(self.target.as_ref(), Some(target))
            && same_target(
                ServerTarget::capture(&self.owner, self.network.as_ref(), cx).as_ref(),
                Some(target),
            )
    }

    /// Writes one preference. The control shows the new value at once. The
    /// server's answer becomes the confirmed state, and a failure puts this
    /// field alone back on the confirmed value and keeps the change for Retry.
    fn save_preference(
        &mut self,
        change: GitPreferenceChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.writes.closing() {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        let Some(loaded) = self.load.ready_mut() else {
            return;
        };
        let shown = loaded.preferences.clone();
        match &change {
            GitPreferenceChange::DefaultIdentity(id) => {
                loaded.preferences.default_identity = id.clone();
            }
            GitPreferenceChange::GitmojiEnabled(enabled) => {
                loaded.preferences.gitmoji_enabled = *enabled;
            }
            GitPreferenceChange::ChangesView(view) => loaded.preferences.changes_view = *view,
        }
        if loaded.preferences == shown {
            return;
        }
        // A new attempt at a field replaces its earlier failure.
        self.failed.retain(|failed| {
            !matches!(failed, FailedWrite::Preference(earlier) if same_field(earlier, &change))
        });
        self.mutations += 1;
        let client = target.client.clone();
        let sent = change.clone();
        let (id, writing) =
            self.writes.submit(
                &target,
                async move { client.save_git_preference(&sent).await },
            );
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(id);
                if !this.current(&target, cx) {
                    return;
                }
                let default_change = match &change {
                    GitPreferenceChange::DefaultIdentity(id) => Some(id.is_some()),
                    _ => None,
                };
                match result {
                    Some(Ok(answer)) => {
                        if let Some(loaded) = this.load.ready_mut() {
                            copy_field(&change, &answer, &mut loaded.preferences);
                            loaded.confirmed = answer;
                        }
                        match default_change {
                            Some(true) => this.toast(
                                "settings.gitIdentities.page.toast.defaultUpdated",
                                None,
                                true,
                                window,
                                cx,
                            ),
                            Some(false) => this.toast(
                                "settings.gitIdentities.page.toast.defaultUnset",
                                None,
                                true,
                                window,
                                cx,
                            ),
                            None => {}
                        }
                    }
                    _ => {
                        if let Some(loaded) = this.load.ready_mut() {
                            let confirmed = loaded.confirmed.clone();
                            copy_field(&change, &confirmed, &mut loaded.preferences);
                        }
                        this.failed.retain(|failed| {
                            !matches!(
                                failed,
                                FailedWrite::Preference(earlier) if same_field(earlier, &change)
                            )
                        });
                        this.failed.push(FailedWrite::Preference(change));
                        this.toast(
                            if default_change.is_some() {
                                "settings.gitIdentities.page.toast.updateDefaultFailed"
                            } else {
                                "settings.common.status.saveFailed"
                            },
                            None,
                            false,
                            window,
                            cx,
                        );
                    }
                }
                cx.notify();
            });
        });
        self.writes.observe(id, completion);
        cx.notify();
    }

    fn toggle_default(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(loaded) = self.load.ready() else {
            return;
        };
        let next =
            (loaded.preferences.default_identity.as_deref() != Some(id)).then(|| id.to_owned());
        self.save_preference(GitPreferenceChange::DefaultIdentity(next), window, cx);
    }

    fn ask_delete(
        &mut self,
        id: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let strings = locale::strings(cx);
        confirm(
            cx.weak_entity(),
            strings.get("settings.gitIdentities.page.deleteDialog.title"),
            strings.format(
                "settings.gitIdentities.page.deleteDialog.description",
                &[("name", &name)],
            ),
            strings.get("settings.common.actions.delete"),
            window,
            cx,
            move |this, window, cx| this.delete(id.clone(), name.clone(), window, cx),
        );
    }

    /// Removes a stored identity. The row stays until the server confirms. An
    /// identity with a delete in flight takes no second one. After a failure
    /// the list is read again, since the server's state is unknown.
    fn delete(&mut self, id: String, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.writes.closing() || self.deleting.contains(&id) {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        self.deleting.insert(id.clone());
        self.failed.retain(
            |failed| !matches!(failed, FailedWrite::Delete { id: earlier, .. } if earlier == &id),
        );
        self.mutations += 1;
        let client = target.client.clone();
        let removed = id.clone();
        let (write, writing) = self.writes.submit(&target, async move {
            client.delete_git_identity(&removed).await
        });
        let completion = cx.spawn_in(window, async move |this, cx| {
            let result = writing.await.ok();
            let _ = this.update_in(cx, |this, window, cx| {
                this.writes.finish(write);
                if !this.current(&target, cx) {
                    return;
                }
                this.deleting.remove(&id);
                if matches!(result, Some(Ok(()))) {
                    if let Some(loaded) = this.load.ready_mut() {
                        loaded.profiles.retain(|profile| profile.id != id);
                        // The store drops a default that named the removed profile.
                        for preferences in [&mut loaded.preferences, &mut loaded.confirmed] {
                            if preferences.default_identity.as_deref() == Some(id.as_str()) {
                                preferences.default_identity = None;
                            }
                        }
                    }
                    this.toast(
                        "settings.gitIdentities.page.toast.profileDeleted",
                        Some(&name),
                        true,
                        window,
                        cx,
                    );
                } else {
                    this.failed.push(FailedWrite::Delete {
                        id: id.clone(),
                        name: name.clone(),
                    });
                    this.toast(
                        "settings.gitIdentities.page.toast.deleteProfileFailed",
                        None,
                        false,
                        window,
                        cx,
                    );
                }
                // Either way the list is read again: after a failure the
                // identity may or may not still exist.
                this.refresh(cx);
                cx.notify();
            });
        });
        self.writes.observe(write, completion);
        cx.notify();
    }

    /// Sends every kept change again.
    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for failed in std::mem::take(&mut self.failed) {
            match failed {
                FailedWrite::Preference(change) => self.save_preference(change, window, cx),
                FailedWrite::Delete { id, name } => self.delete(id, name, window, cx),
            }
        }
    }

    pub(crate) fn take_pending_write(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let pending = self.writes.take_pending_write();
        cx.notify();
        pending
    }

    pub(crate) fn has_failed_writes(&self) -> bool {
        !self.failed.is_empty()
    }

    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.writes.cancel_close();
        cx.notify();
    }

    pub(crate) fn take_quit_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        self.writes.take_quit_write(self.network.take(), cx)
    }

    /// Scrolls a Settings search result's control into view.
    pub(crate) fn reveal(&mut self, item: &str, window: &mut Window, cx: &mut Context<Self>) {
        let index = match item {
            "git.identities" => IDENTITIES,
            "git.changes-view" => CHANGES_VIEW,
            "git.gitmoji" => GITMOJI,
            _ => return,
        };
        cx.on_next_frame(window, move |this, window, cx| {
            this.anchors[index].scroll_to(window, cx);
        });
    }

    fn text_button(
        &mut self,
        id: SharedString,
        label: SharedString,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        on_activate(
            gpui_kit::base::Button::new(id.clone())
                .accessibility_id(id)
                .accessibility_label(label.clone())
                .disabled(self.writes.closing())
                .h(gpui_kit::rems(1.75))
                .px(px(8.0))
                .rounded(px(7.0))
                .child(label),
            cx,
            action,
        )
        .into_any_element()
    }

    fn identity_row(
        &mut self,
        identity: &Identity,
        is_default: bool,
        bordered: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let read_only = identity.is_system();
        let name: SharedString = if identity.name.is_empty() {
            identity.user_name.clone().into()
        } else {
            identity.name.clone().into()
        };
        // What keeps a stored identity from being chosen replaces its email.
        let incomplete = !read_only && !identity.is_complete();
        let detail: SharedString = if incomplete {
            strings.get("settings.gitIdentities.page.incomplete")
        } else {
            identity.user_email.clone().into()
        };
        let default_id = identity.id.clone();
        let toggle = self.text_button(
            format!("git-identity-default-{}", identity.id).into(),
            strings.get(if is_default {
                "settings.gitIdentities.page.actions.unsetDefault"
            } else {
                "settings.gitIdentities.page.actions.setAsDefault"
            }),
            cx,
            move |this, window, cx| this.toggle_default(&default_id, window, cx),
        );
        let delete = (!read_only).then(|| {
            let (id, deleted_name) = (identity.id.clone(), name.to_string());
            self.text_button(
                format!("git-identity-delete-{}", identity.id).into(),
                strings.get("settings.common.actions.delete"),
                cx,
                move |this, window, cx| {
                    this.ask_delete(id.clone(), deleted_name.clone(), window, cx)
                },
            )
        });
        let row_id: SharedString = format!("git-identity-{}", identity.id).into();
        div()
            .id(row_id.clone())
            .accessibility_id(row_id)
            .role(Role::Group)
            .aria_label(name.clone())
            .flex_row_from(direction)
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .px(px(16.0))
            .py(px(10.0))
            .when(bordered, |row| {
                row.border_b_1().border_color(palette.border)
            })
            .child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(12.0))
                    .min_w_0()
                    .child(glyph("git-branch", 16.0, palette.muted_foreground))
                    .child(
                        div()
                            .min_w_0()
                            .child(
                                div()
                                    .flex_row_from(direction)
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(div().truncate().child(name))
                                    .children(is_default.then(|| {
                                        div()
                                            .flex_shrink_0()
                                            .px(px(4.0))
                                            .rounded(px(4.0))
                                            .css_text(0.6875, 1.0, cx)
                                            .text_color(palette.selection_foreground)
                                            .bg(palette.selection)
                                            .child(
                                                strings.get(
                                                    "settings.gitIdentities.page.badge.default",
                                                ),
                                            )
                                    })),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .css_text(0.6875, 1.0, cx)
                                    .text_color(if incomplete {
                                        palette.status_warning
                                    } else {
                                        palette.muted_foreground
                                    })
                                    .child(detail),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .child(toggle)
                    .children(delete),
            )
            .test_support()
            .into_any_element()
    }

    fn view_chip(
        &mut self,
        view: ChangesView,
        current: ChangesView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let (id, option): (SharedString, _) = match view {
            ChangesView::Flat => (
                "git-changes-view-flat".into(),
                strings.get("settings.openchamber.git.option.flatList"),
            ),
            ChangesView::Tree => (
                "git-changes-view-tree".into(),
                strings.get("settings.openchamber.git.option.treeView"),
            ),
        };
        let selected = view == current;
        let focus = focus_handle(&mut self.focus, &id, cx);
        let hover = palette.hover;
        on_activate(
            div()
                .id(id.clone())
                .accessibility_id(id)
                .role(Role::Button)
                .aria_label(strings.format(
                    "settings.openchamber.git.optionAria",
                    &[("option", &option)],
                ))
                .aria_toggled(if selected {
                    Toggled::True
                } else {
                    Toggled::False
                })
                .track_focus(&focus)
                .tab_index(0)
                .h_full()
                .px(px(10.0))
                .rounded(px(5.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .when(selected, |chip| {
                    chip.bg(palette.background).font_weight(FontWeight::MEDIUM)
                })
                .when(!selected, |chip| chip.hover(move |style| style.bg(hover)))
                .child(option),
            cx,
            move |this, window, cx| {
                this.save_preference(GitPreferenceChange::ChangesView(view), window, cx)
            },
        )
        .test_support()
        .into_any_element()
    }
}

impl Render for GitSettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.retry_requested) {
            self.retry(window, cx);
        }
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let frame = page_frame("git-settings-page", Page::Git, &self.scroll, cx);
        let loaded = match (&self.target, &self.load) {
            (None, _) => {
                return frame.child(crate::settings_controls::helper(
                    strings.get("common.unavailable"),
                    cx,
                ));
            }
            (_, Load::Loading) => return frame,
            (_, Load::Failed) => {
                return frame.child(load_failed(
                    "git-settings-load-failed",
                    strings.get("settings.openchamber.desktopNetwork.error.loadFailed"),
                    strings.get("desktopHostSwitcher.actions.retry"),
                    cx,
                    |this, cx| this.refresh(cx),
                ));
            }
            (_, Load::Ready(loaded)) => loaded.clone(),
        };
        let default = loaded.preferences.default_identity.clone();
        let mut list = div()
            .id("git-identities-list")
            .rounded(px(10.0))
            .bg(palette.elevated)
            .overflow_hidden()
            .flex()
            .flex_col();
        let count = loaded.profiles.len();
        if let Some(system) = &loaded.system {
            let is_default = default.as_deref() == Some(system.id.as_str());
            list = list.child(self.identity_row(system, is_default, count > 0, cx));
        }
        for (index, profile) in loaded.profiles.iter().enumerate() {
            let is_default = default.as_deref() == Some(profile.id.as_str());
            list = list.child(self.identity_row(profile, is_default, index + 1 < count, cx));
        }
        if loaded.system.is_none() && count == 0 {
            list = list.child(
                div()
                    .id("git-identities-empty")
                    .py(px(32.0))
                    .px(px(16.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.0))
                    .text_color(palette.muted_foreground)
                    .child(glyph("shield-keyhole", 32.0, palette.muted_foreground))
                    .child(strings.get("settings.gitIdentities.page.empty.title"))
                    .child(
                        div()
                            .css_text(0.8125, 1.25, cx)
                            .child(strings.get("settings.gitIdentities.page.empty.description")),
                    ),
            );
        }
        let failed = (!self.failed.is_empty()).then(|| {
            load_failed(
                "git-settings-save-failed",
                strings.get("settings.common.status.saveFailed"),
                strings.get("desktopHostSwitcher.actions.retry"),
                cx,
                |this, cx| {
                    this.retry_requested = true;
                    cx.notify();
                },
            )
        });
        let current_view = loaded.preferences.changes_view;
        let flat = self.view_chip(ChangesView::Flat, current_view, cx);
        let tree = self.view_chip(ChangesView::Tree, current_view, cx);
        let view_group = div()
            .id("git-changes-view-group")
            .role(Role::Group)
            .aria_label(strings.get("settings.openchamber.git.changesViewAria"))
            .flex_row_from(direction)
            .items_center()
            .gap(px(2.0))
            .p(px(2.0))
            .h(gpui_kit::rems(2.0))
            .rounded(px(7.0))
            .bg(palette.elevated)
            .child(flat)
            .child(tree)
            .into_any_element();
        let gitmoji_focus = focus_handle(&mut self.focus, &"git-gitmoji".into(), cx);
        let gitmoji = checkbox_row(
            "git-gitmoji".into(),
            strings.get("settings.openchamber.git.enableGitmoji"),
            strings.get("settings.openchamber.git.enableGitmojiAria"),
            loaded.preferences.gitmoji_enabled,
            !self.writes.closing(),
            &gitmoji_focus,
            Some(self.anchors[GITMOJI].clone()),
            cx,
            |this: &mut Self, checked, window, cx| {
                this.save_preference(GitPreferenceChange::GitmojiEnabled(checked), window, cx)
            },
        );
        frame
            .children(failed.map(|failed| div().mb(px(16.0)).child(failed)))
            .child(section_with_info(
                "settings.gitIdentities.page.section.title",
                None,
                true,
                vec![list.into_any_element()],
                Some(self.anchors[IDENTITIES].clone()),
                cx,
            ))
            .child(section_with_info(
                "settings.openchamber.git.title",
                None,
                false,
                vec![
                    field_row(
                        "git.changes-view",
                        strings.get("settings.openchamber.git.changesViewTitle"),
                        None,
                        view_group,
                        Some(self.anchors[CHANGES_VIEW].clone()),
                        cx,
                    ),
                    gitmoji,
                ],
                None,
                cx,
            ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext};
    use serde_json::{Value, json};

    use super::*;
    use crate::settings_controls::tests::{ResponseGate, retarget};
    use crate::settings_test_server::{Server, open, settings, settle};

    struct Fixture {
        server: Server,
        document: Arc<Mutex<Value>>,
        profiles: Arc<Mutex<Vec<Value>>>,
        fail_reads: Arc<AtomicBool>,
        fail_system: Arc<AtomicBool>,
        fail_writes: Arc<AtomicBool>,
    }

    /// A server with the machine's author, one stored identity `work` and a
    /// settings document.
    fn fixture(profile: &'static str) -> Fixture {
        gated_fixture(profile, None)
    }

    /// `gate` holds every delete request.
    fn gated_fixture(profile: &'static str, gate: Option<Arc<ResponseGate>>) -> Fixture {
        let document = Arc::new(Mutex::new(json!({"gitChangesViewMode": "flat"})));
        let profiles = Arc::new(Mutex::new(vec![json!({
            "id": profile,
            "name": "Work",
            "userName": "Dev",
            "userEmail": "dev@example.test",
            "transport": "anonymous",
        })]));
        let fail_reads = Arc::new(AtomicBool::new(false));
        let fail_writes = Arc::new(AtomicBool::new(false));
        let fail_system = Arc::new(AtomicBool::new(false));
        let (stored, system_fails) = (Arc::clone(&profiles), Arc::clone(&fail_system));
        let (held, reads, writes) = (
            Arc::clone(&document),
            Arc::clone(&fail_reads),
            Arc::clone(&fail_writes),
        );
        let server = Server::new(move |request| {
            let path = request.target.split('?').next().unwrap_or_default();
            if request.method == "GET" {
                if reads.load(Ordering::Acquire) {
                    return (500, json!({"error": "fixture read failure"}));
                }
                return match path {
                    "/api/git/identities" => (
                        200,
                        Value::Array(profiles.lock().expect("profiles").clone()),
                    ),
                    "/api/git/global-identity" if system_fails.load(Ordering::Acquire) => {
                        (500, json!({"error": "fixture read failure"}))
                    }
                    "/api/git/global-identity" => (
                        200,
                        json!({"userName": "Machine", "userEmail": "machine@example.test"}),
                    ),
                    _ => (200, held.lock().expect("document").clone()),
                };
            }
            if request.method == "DELETE"
                && let Some(gate) = &gate
            {
                gate.hold();
            }
            if writes.load(Ordering::Acquire) {
                return (500, json!({"error": "fixture write failure"}));
            }
            if request.method == "DELETE" {
                let id = path.trim_start_matches("/api/git/identities/").to_owned();
                profiles
                    .lock()
                    .expect("profiles")
                    .retain(|profile| profile["id"] != id.as_str());
                return (200, json!({"success": true}));
            }
            let mut document = held.lock().expect("document");
            if let Some(changes) = request.body.as_object() {
                for (key, value) in changes {
                    document[key.as_str()] = value.clone();
                }
            }
            (200, document.clone())
        });
        Fixture {
            server,
            document,
            profiles: stored,
            fail_reads,
            fail_system,
            fail_writes,
        }
    }

    fn page(workspace: &Entity<Workspace>, cx: &TestAppContext) -> Entity<GitSettingsPage> {
        let settings = settings(workspace, cx);
        cx.read(|cx| settings.read(cx).git.clone())
    }

    fn act(
        window: AnyWindowHandle,
        page: &Entity<GitSettingsPage>,
        cx: &mut TestAppContext,
        action: impl FnOnce(&mut GitSettingsPage, &mut Window, &mut Context<GitSettingsPage>),
    ) {
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| action(page, window, cx));
        })
        .expect("test window");
        settle(window, cx);
    }

    fn loaded(page: &Entity<GitSettingsPage>, cx: &TestAppContext) -> Loaded {
        cx.read(|cx| page.read(cx).load.ready().cloned().expect("loaded"))
    }

    #[gpui_kit::test]
    fn identities_and_preferences_load_together(cx: &mut TestAppContext) {
        let fixture = fixture("work");
        let (window, workspace) = open(&fixture.server, Page::Git, cx);
        let page = page(&workspace, cx);
        let loaded = loaded(&page, cx);
        assert_eq!(loaded.profiles.len(), 1);
        assert_eq!(
            loaded.system.map(|system| system.user_name).as_deref(),
            Some("Machine")
        );
        assert_eq!(loaded.preferences.default_identity, None);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("git-identity-work").is_some());
            assert!(window.try_find("git-identity-global").is_some());
            assert!(
                window.try_find("git-identity-delete-global").is_none(),
                "the machine's own author cannot be deleted"
            );
            assert!(window.try_find("git-identities-empty").is_none());
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn a_failed_read_offers_retry_and_no_empty_list(cx: &mut TestAppContext) {
        let fixture = fixture("work");
        fixture.fail_reads.store(true, Ordering::Release);
        let (window, workspace) = open(&fixture.server, Page::Git, cx);
        let page = page(&workspace, cx);
        cx.read(|cx| assert_eq!(page.read(cx).load, Load::Failed));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("git-settings-load-failed-retry").is_some());
            assert!(window.try_find("git-identities-empty").is_none());
            assert!(window.try_find("git-gitmoji").is_none());
        })
        .expect("test window");
        fixture.fail_reads.store(false, Ordering::Release);
        act(window, &page, cx, |page, _, cx| page.refresh(cx));
        assert_eq!(loaded(&page, cx).profiles.len(), 1);
    }

    #[gpui_kit::test]
    fn the_default_toggles_and_preferences_write_one_field(cx: &mut TestAppContext) {
        let fixture = fixture("work");
        let (window, workspace) = open(&fixture.server, Page::Git, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, window, cx| {
            page.toggle_default("work", window, cx)
        });
        assert_eq!(
            fixture.server.writes()[0].body,
            json!({"defaultGitIdentityId": "work"})
        );
        assert_eq!(
            loaded(&page, cx).preferences.default_identity.as_deref(),
            Some("work")
        );
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).notice.as_deref(),
                Some("Default identity updated")
            );
        });
        act(window, &page, cx, |page, window, cx| {
            page.toggle_default("work", window, cx)
        });
        assert_eq!(
            fixture.server.writes()[1].body,
            json!({"defaultGitIdentityId": ""})
        );
        act(window, &page, cx, |page, window, cx| {
            page.save_preference(GitPreferenceChange::GitmojiEnabled(true), window, cx);
        });
        act(window, &page, cx, |page, window, cx| {
            page.save_preference(
                GitPreferenceChange::ChangesView(ChangesView::Tree),
                window,
                cx,
            );
            // The value already shown writes nothing.
            page.save_preference(
                GitPreferenceChange::ChangesView(ChangesView::Tree),
                window,
                cx,
            );
        });
        assert_eq!(fixture.server.writes().len(), 4);
        let document = fixture.document.lock().expect("document").clone();
        assert_eq!(document["gitmojiEnabled"], true);
        assert_eq!(document["gitChangesViewMode"], "tree");
    }

    #[gpui_kit::test]
    fn a_failed_write_restores_the_server_value_and_retry_sends_it(cx: &mut TestAppContext) {
        let fixture = fixture("work");
        let (window, workspace) = open(&fixture.server, Page::Git, cx);
        let page = page(&workspace, cx);
        fixture.fail_writes.store(true, Ordering::Release);
        act(window, &page, cx, |page, window, cx| {
            page.save_preference(GitPreferenceChange::GitmojiEnabled(true), window, cx);
        });
        assert!(
            !loaded(&page, cx).preferences.gitmoji_enabled,
            "the checkbox shows what the server holds"
        );
        cx.read(|cx| {
            let page = page.read(cx);
            assert!(page.has_failed_writes());
            assert_eq!(
                page.failed,
                [FailedWrite::Preference(
                    GitPreferenceChange::GitmojiEnabled(true)
                )]
            );
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("git-settings-save-failed-retry").is_some());
        })
        .expect("test window");
        fixture.fail_writes.store(false, Ordering::Release);
        act(window, &page, cx, |page, window, cx| page.retry(window, cx));
        assert!(loaded(&page, cx).preferences.gitmoji_enabled);
        cx.read(|cx| assert!(!page.read(cx).has_failed_writes()));
    }

    #[gpui_kit::test]
    fn deleting_a_profile_removes_its_row_and_a_failed_delete_keeps_it(cx: &mut TestAppContext) {
        let fixture = fixture("work");
        let (window, workspace) = open(&fixture.server, Page::Git, cx);
        let page = page(&workspace, cx);
        fixture.fail_writes.store(true, Ordering::Release);
        act(window, &page, cx, |page, window, cx| {
            page.delete("work".into(), "Work".into(), window, cx)
        });
        assert_eq!(loaded(&page, cx).profiles.len(), 1, "the row stays");
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).notice.as_deref(),
                Some("Failed to delete profile")
            );
        });
        fixture.fail_writes.store(false, Ordering::Release);
        act(window, &page, cx, |page, window, cx| page.retry(window, cx));
        assert!(loaded(&page, cx).profiles.is_empty());
        cx.read(|cx| {
            assert_eq!(
                page.read(cx).notice.as_deref(),
                Some("Profile \"Work\" deleted")
            );
        });
    }

    fn identity_reads(fixture: &Fixture) -> usize {
        fixture
            .server
            .requests()
            .iter()
            .filter(|request| request.is("GET", "/api/git/identities"))
            .count()
    }

    #[gpui_kit::test]
    fn two_failed_writes_each_restore_their_own_confirmed_field(cx: &mut TestAppContext) {
        let fixture = fixture("work");
        let (window, workspace) = open(&fixture.server, Page::Git, cx);
        let page = page(&workspace, cx);
        fixture.fail_writes.store(true, Ordering::Release);
        // Both changes are in flight before either answer arrives.
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.save_preference(GitPreferenceChange::GitmojiEnabled(true), window, cx);
                page.save_preference(
                    GitPreferenceChange::ChangesView(ChangesView::Tree),
                    window,
                    cx,
                );
            });
        })
        .expect("test window");
        settle(window, cx);
        let shown = loaded(&page, cx);
        assert_eq!(
            shown.preferences, shown.confirmed,
            "neither failure leaves the other's unsaved value on screen"
        );
        assert!(!shown.preferences.gitmoji_enabled);
        assert_eq!(shown.preferences.changes_view, ChangesView::Flat);
        cx.read(|cx| assert_eq!(page.read(cx).failed.len(), 2));
        fixture.fail_writes.store(false, Ordering::Release);
        act(window, &page, cx, |page, window, cx| page.retry(window, cx));
        let shown = loaded(&page, cx);
        assert!(shown.preferences.gitmoji_enabled);
        assert_eq!(shown.preferences.changes_view, ChangesView::Tree);
        assert_eq!(shown.preferences, shown.confirmed);
        cx.read(|cx| assert!(!page.read(cx).has_failed_writes()));
    }

    #[gpui_kit::test]
    fn a_failed_read_of_the_machine_identity_fails_the_page(cx: &mut TestAppContext) {
        let fixture = fixture("work");
        fixture.fail_system.store(true, Ordering::Release);
        let (window, workspace) = open(&fixture.server, Page::Git, cx);
        let page = page(&workspace, cx);
        cx.read(|cx| assert_eq!(page.read(cx).load, Load::Failed));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("git-settings-load-failed-retry").is_some());
            assert!(
                window.try_find("git-identity-work").is_none(),
                "the list is not shown as if the machine had no author"
            );
        })
        .expect("test window");
        fixture.fail_system.store(false, Ordering::Release);
        act(window, &page, cx, |page, _, cx| page.refresh(cx));
        assert!(loaded(&page, cx).system.is_some());
    }

    #[gpui_kit::test]
    fn a_delete_in_flight_refuses_a_second_one(cx: &mut TestAppContext) {
        let gate = ResponseGate::new();
        let fixture = gated_fixture("work", Some(Arc::clone(&gate)));
        let (window, workspace) = open(&fixture.server, Page::Git, cx);
        let page = page(&workspace, cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.delete("work".into(), "Work".into(), window, cx);
                page.delete("work".into(), "Work".into(), window, cx);
            });
        })
        .expect("test window");
        gate.entered(cx);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.delete("work".into(), "Work".into(), window, cx);
            });
        })
        .expect("test window");
        gate.release();
        settle(window, cx);
        assert_eq!(
            fixture.server.writes().len(),
            1,
            "one delete for three presses"
        );
        assert!(loaded(&page, cx).profiles.is_empty());
        cx.read(|cx| assert!(page.read(cx).deleting.is_empty()));
    }

    #[gpui_kit::test]
    fn a_failed_delete_rereads_the_list_and_forgets_a_row_that_is_gone(cx: &mut TestAppContext) {
        let fixture = fixture("work");
        let (window, workspace) = open(&fixture.server, Page::Git, cx);
        let page = page(&workspace, cx);
        fixture.fail_writes.store(true, Ordering::Release);
        let before = identity_reads(&fixture);
        act(window, &page, cx, |page, window, cx| {
            page.delete("work".into(), "Work".into(), window, cx)
        });
        assert_eq!(
            identity_reads(&fixture) - before,
            1,
            "the list is read again after the failure"
        );
        cx.read(|cx| assert!(page.read(cx).has_failed_writes()));
        // The identity is removed elsewhere before Retry.
        fixture.profiles.lock().expect("profiles").clear();
        act(window, &page, cx, |page, _, cx| page.refresh(cx));
        assert!(loaded(&page, cx).profiles.is_empty());
        cx.read(|cx| {
            assert!(
                !page.read(cx).has_failed_writes(),
                "nothing is left to retry for a row that is gone"
            );
        });
    }

    #[gpui_kit::test]
    fn a_hidden_page_reads_nothing_when_the_connection_changes(cx: &mut TestAppContext) {
        let first = fixture("old-server-profile");
        let second = fixture("work");
        let (window, workspace) = open(&first.server, Page::Git, cx);
        let page = page(&workspace, cx);
        act(window, &page, cx, |page, _, cx| page.set_shown(false, cx));
        let reads = cx.read(|cx| page.read(cx).read_revision);
        retarget(&workspace, &second.server, cx);
        settle(window, cx);
        cx.read(|cx| {
            let page = page.read(cx);
            assert_eq!(page.read_revision, reads, "a hidden page starts no read");
            assert_eq!(page.load, Load::Loading);
        });
        act(window, &page, cx, |page, _, cx| page.set_shown(true, cx));
        assert_eq!(loaded(&page, cx).profiles[0].id, "work");
    }

    #[gpui_kit::test]
    fn a_runtime_switch_shows_only_the_new_servers_identities(cx: &mut TestAppContext) {
        let first = fixture("old-server-profile");
        let second = fixture("work");
        let (window, workspace) = open(&first.server, Page::Git, cx);
        let page = page(&workspace, cx);
        first.fail_writes.store(true, Ordering::Release);
        cx.update_window(window, |_, window, cx| {
            page.update(cx, |page, cx| {
                page.save_preference(GitPreferenceChange::GitmojiEnabled(true), window, cx);
            });
        })
        .expect("test window");
        retarget(&workspace, &second.server, cx);
        settle(window, cx);
        let loaded = loaded(&page, cx);
        assert_eq!(loaded.profiles[0].id, "work");
        cx.read(|cx| {
            assert!(
                !page.read(cx).has_failed_writes(),
                "the retired server's failed write is not offered for the new one"
            );
        });
        assert!(second.server.writes().is_empty());
    }
}
