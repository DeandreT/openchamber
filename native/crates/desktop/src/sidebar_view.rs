//! Compact sidebar rows over the core row model.

use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, div, list, prelude::FluentBuilder,
    px, rems, svg,
};
use openchamber_native_core::grouping::GroupLabel;
use openchamber_native_core::sidebar_rows::{ActivityKey, EmptyKind, GroupLoadState, ShowControl};

use openchamber_native_core::session_index::IndexStatus;

use crate::activation::on_activate;
use crate::assets;
use crate::connection::{ConnectionFailure, ConnectionState};
use crate::locale::{self, Strings};
use crate::sidebar_item::SidebarItem;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// Default sidebar width of the desktop layout.
pub const SIDEBAR_WIDTH: f32 = 280.0;
const HEADER_HEIGHT: f32 = 32.0;
const SESSION_HEIGHT: f32 = 28.0;
const NOTICE_HEIGHT: f32 = 28.0;
/// Tree geometry: base padding, per-level indent, chevron slot and gap.
const TREE_BASE_PADDING: f32 = 6.0;
const TREE_INDENT: f32 = 12.0;
const CHEVRON_SLOT: f32 = 14.0;
const CHEVRON_GAP: f32 = 6.0;
/// `--text-ui-label` and `--text-meta` of the design system.
const LABEL_SIZE: f32 = 0.843_75;
const META_SIZE: f32 = 0.781_25;

impl Workspace {
    pub(crate) fn render_sidebar(
        &self,
        palette: &Palette,
        right_to_left: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows = list(
            self.sidebar_list.clone(),
            cx.processor(|this, index: usize, window, cx| {
                this.render_sidebar_row(index, window, cx)
            }),
        )
        .flex_1()
        .min_h_0();
        let status = self.connection_status(&locale::strings(cx).clone());
        div()
            .id("sidebar")
            .role(Role::Tree)
            .accessibility_id("sidebar")
            .w(px(SIDEBAR_WIDTH))
            .flex_shrink_0()
            .h_full()
            .bg(palette.muted)
            .border_color(palette.border)
            .when(right_to_left, |this| this.border_l_1())
            .when(!right_to_left, |this| this.border_r_1())
            .py_1()
            .flex()
            .flex_col()
            .when_some(status, |this, status| {
                this.child(
                    div()
                        .id("connection-status")
                        .role(Role::Status)
                        .accessibility_id("connection-status")
                        .aria_label(status.clone())
                        .flex_shrink_0()
                        .px(px(TREE_BASE_PADDING + CHEVRON_GAP))
                        .py_1()
                        .text_size(rems(META_SIZE))
                        .text_color(palette.muted_foreground)
                        .child(status),
                )
            })
            .child(rows)
    }

    /// What the sidebar says about the server, while there is something to say.
    fn connection_status(&self, strings: &Strings) -> Option<SharedString> {
        let connection = self.connection.as_ref()?;
        let host = connection.config.endpoint.authority();
        let text = match connection.state {
            ConnectionState::Connecting => strings.format(
                "desktopHostSwitcher.startup.connectingTo",
                &[("host", &host)],
            ),
            ConnectionState::Reconnecting => strings.get("opencodeCompatibility.reconnecting"),
            ConnectionState::Failed(failure) => failure_text(strings, failure, &host),
            ConnectionState::Connected => match self.sessions.status() {
                IndexStatus::Loading if self.sessions.active().is_empty() => {
                    strings.get("sessions.sidebar.group.empty.loadingSessions")
                }
                IndexStatus::Failed => strings.get("sessions.sidebar.group.empty.loadFailed"),
                _ => return None,
            },
        };
        Some(text)
    }

    fn render_sidebar_row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(item) = self.sidebar_items.get(index).cloned() else {
            return div().into_any_element();
        };
        let palette = cx.global::<Palette>().clone();
        let strings = locale::strings(cx).clone();
        match &item {
            SidebarItem::Zone {
                key,
                activity,
                collapsed,
            } => {
                let label = zone_label(&strings, *activity);
                let row = header_row(key, &palette, *collapsed, label, None);
                on_activate(row, cx, move |this, _, cx| this.toggle_collapsed(&item, cx))
                    .test_support()
                    .into_any_element()
            }
            SidebarItem::Project {
                key,
                label,
                collapsed,
                ..
            } => {
                let row = header_row(key, &palette, *collapsed, label.clone(), Some("folder"))
                    .text_color(palette.foreground);
                on_activate(row, cx, move |this, _, cx| this.toggle_collapsed(&item, cx))
                    .test_support()
                    .into_any_element()
            }
            SidebarItem::Group {
                key,
                label,
                collapsed,
                empty,
                ..
            } => {
                let (text, icon) = group_label(&strings, label);
                let slot = if *empty {
                    div().into_any_element()
                } else {
                    chevron(*collapsed, &palette).into_any_element()
                };
                let row = tree_row(key, &palette, 1, slot, false)
                    .role(Role::Button)
                    .aria_label(text.clone())
                    .aria_expanded(!*collapsed)
                    .h(px(SESSION_HEIGHT))
                    .child(icon_element(icon, &palette, palette.muted_foreground))
                    .child(row_text(text, palette.muted_foreground));
                on_activate(row, cx, move |this, _, cx| this.toggle_collapsed(&item, cx))
                    .test_support()
                    .into_any_element()
            }
            SidebarItem::Session {
                key,
                session_id,
                title,
                depth,
                has_children,
                expanded,
                expansion_key,
                pinned,
                ..
            } => {
                let selected = self.selected.as_ref() == Some(session_id);
                let streaming = self.is_streaming(session_id);
                let id = session_id.clone();
                let slot = if *has_children {
                    let expansion_key = expansion_key.clone();
                    let label = if *expanded {
                        strings.get("sessions.sidebar.session.subsessions.collapse")
                    } else {
                        strings.get("sessions.sidebar.session.subsessions.expand")
                    };
                    let button = div()
                        .id(SharedString::from(format!("{key}:chevron")))
                        .role(Role::Button)
                        .aria_label(label)
                        .aria_expanded(*expanded)
                        .child(chevron(!*expanded, &palette));
                    on_activate(button, cx, move |this, _, cx| {
                        cx.stop_propagation();
                        this.toggle_expanded(&expansion_key, cx);
                    })
                    .test_support()
                    .into_any_element()
                } else {
                    div().into_any_element()
                };
                let title = if title.is_empty() {
                    strings.get("sessions.sidebar.session.untitled")
                } else {
                    title.clone()
                };
                let text_color = if selected || streaming {
                    palette.foreground
                } else {
                    palette.muted_foreground
                };
                let row = tree_row(key, &palette, *depth, slot, selected)
                    .role(Role::TreeItem)
                    .aria_label(title.clone())
                    .aria_selected(selected)
                    .aria_level(depth + 1)
                    .when(*has_children, |this| this.aria_expanded(*expanded))
                    .h(px(SESSION_HEIGHT))
                    .child(row_text(title, text_color))
                    .when(streaming, |this| {
                        this.child(icon_element("circle", &palette, palette.info))
                    })
                    .when(*pinned && !streaming, |this| {
                        this.child(icon_element("pushpin", &palette, palette.muted_foreground))
                    });
                on_activate(row, cx, move |this, window, cx| {
                    this.select_session(id.clone(), window, cx);
                })
                .test_support()
                .into_any_element()
            }
            SidebarItem::Show {
                key,
                control,
                depth,
                ..
            } => {
                let label = match control {
                    ShowControl::More => strings.get("sessions.sidebar.group.showMore"),
                    ShowControl::Fewer => strings.get("sessions.sidebar.group.showFewer"),
                };
                let row = notice_row(key, *depth, label.clone(), palette.muted_foreground)
                    .role(Role::Button)
                    .aria_label(label)
                    .cursor_pointer()
                    .hover(|style| style.text_color(palette.foreground));
                on_activate(row, cx, move |this, _, cx| this.reveal(&item, cx))
                    .test_support()
                    .into_any_element()
            }
            SidebarItem::Status { key, state, depth } => {
                let label = status_label(&strings, *state);
                let color = match state {
                    GroupLoadState::Loading | GroupLoadState::Ready => palette.muted_foreground,
                    _ => palette.error,
                };
                notice_row(key, *depth, label, color).into_any_element()
            }
            SidebarItem::Empty { key, kind, depth } => match kind {
                EmptyKind::Archived => notice_row(
                    key,
                    *depth,
                    strings.get("sessions.sidebar.group.empty.noArchivedSessions"),
                    palette.muted_foreground,
                )
                .into_any_element(),
                EmptyKind::Sidebar => div()
                    .id(key.clone())
                    .px_3()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(row_text(
                        strings.get("sessions.sidebar.empty.noSessions.title"),
                        palette.foreground,
                    ))
                    .child(
                        div()
                            .text_size(rems(META_SIZE))
                            .text_color(palette.muted_foreground)
                            .child(strings.get("sessions.sidebar.empty.noSessions.description")),
                    )
                    .into_any_element(),
            },
        }
    }
}

fn zone_label(strings: &Strings, activity: ActivityKey) -> SharedString {
    match activity {
        ActivityKey::Chats => strings.get("sessions.sidebar.activity.chatsTitle"),
        ActivityKey::Pinned => strings.get("sessions.sidebar.activity.pinnedTitle"),
        ActivityKey::Work => strings.get("sessions.sidebar.work.title"),
    }
}

fn group_label(strings: &Strings, label: &GroupLabel) -> (SharedString, &'static str) {
    match label {
        GroupLabel::Chats => (
            strings.get("sessions.sidebar.activity.chatsTitle"),
            "folder",
        ),
        GroupLabel::ProjectRoot {
            branch: Some(branch),
        } => (
            strings.format(
                "sessions.sidebar.grouping.projectRootWithBranch",
                &[("branch", branch)],
            ),
            "git-branch",
        ),
        GroupLabel::ProjectRoot { branch: None } => (
            strings.get("sessions.sidebar.grouping.projectRoot"),
            "folder",
        ),
        GroupLabel::Worktree { label } => (label.clone().into(), "git-branch"),
        GroupLabel::Space { name: Some(name) } => (name.clone().into(), "box-3"),
        GroupLabel::Space { name: None } => (
            strings.get("sessions.sidebar.grouping.spaceUnnamed"),
            "box-3",
        ),
        GroupLabel::Archived => (strings.get("sessions.sidebar.grouping.archived"), "archive"),
    }
}

fn status_label(strings: &Strings, state: GroupLoadState) -> SharedString {
    match state {
        GroupLoadState::Ready | GroupLoadState::Loading => {
            strings.get("sessions.sidebar.group.empty.loadingSessions")
        }
        GroupLoadState::LoadFailed => strings.get("sessions.sidebar.group.empty.loadFailed"),
        GroupLoadState::InitializationFailed => {
            strings.get("sessions.sidebar.group.empty.initializationFailed")
        }
        GroupLoadState::PermissionDenied => {
            strings.get("sessions.sidebar.group.empty.permissionDenied")
        }
    }
}

fn header_row(
    key: &SharedString,
    palette: &Palette,
    collapsed: bool,
    label: SharedString,
    icon: Option<&'static str>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(key.clone())
        .role(Role::Button)
        .accessibility_id(key.clone())
        .aria_label(label.clone())
        .aria_expanded(!collapsed)
        .h(px(HEADER_HEIGHT))
        .px(px(TREE_BASE_PADDING))
        .flex()
        .items_center()
        .gap(px(CHEVRON_GAP))
        .cursor_pointer()
        .text_color(palette.muted_foreground)
        .hover(|style| style.bg(palette.hover))
        .child(chevron(collapsed, palette))
        .when_some(icon, |this, icon| {
            this.child(icon_element(icon, palette, palette.muted_foreground))
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(rems(LABEL_SIZE))
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .child(label),
        )
}

fn chevron(collapsed: bool, palette: &Palette) -> impl IntoElement {
    let name = if collapsed {
        "arrow-right-s"
    } else {
        "arrow-down-s"
    };
    icon_element(name, palette, palette.muted_foreground)
}

/// A tree row with its chevron slot; `slot` is empty for leaf rows.
fn tree_row(
    key: &SharedString,
    palette: &Palette,
    depth: usize,
    slot: AnyElement,
    selected: bool,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let padding = TREE_BASE_PADDING + depth as f32 * TREE_INDENT;
    div()
        .id(key.clone())
        .accessibility_id(key.clone())
        .mx_1()
        .rounded(px(6.0))
        .pl(px(padding))
        .pr_2()
        .flex()
        .items_center()
        .gap(px(CHEVRON_GAP))
        .cursor_pointer()
        .when(selected, |this| {
            this.bg(palette.selection)
                .text_color(palette.selection_foreground)
        })
        .when(!selected, |this| {
            this.hover(|style| style.bg(palette.hover))
        })
        .child(
            div()
                .w(px(CHEVRON_SLOT))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .child(slot),
        )
}

fn notice_row(
    key: &SharedString,
    depth: usize,
    label: SharedString,
    color: gpui_kit::Hsla,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let padding = TREE_BASE_PADDING + depth as f32 * TREE_INDENT + CHEVRON_SLOT + CHEVRON_GAP;
    div()
        .id(key.clone())
        .accessibility_id(key.clone())
        .h(px(NOTICE_HEIGHT))
        .pl(px(padding + 4.0))
        .flex()
        .items_center()
        .text_size(rems(META_SIZE))
        .text_color(color)
        .child(label)
}

fn row_text(text: SharedString, color: gpui_kit::Hsla) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .truncate()
        .text_size(rems(LABEL_SIZE))
        .text_color(color)
        .child(text)
}

/// A 16 px sprite icon in `color`.
pub fn icon_element(name: &str, _palette: &Palette, color: gpui_kit::Hsla) -> impl IntoElement {
    svg()
        .path(assets::icon_path(name))
        .size(px(16.0))
        .flex_shrink_0()
        .text_color(color)
}

/// The sentence that explains a connection failure.
pub(crate) fn failure_text(
    strings: &Strings,
    failure: ConnectionFailure,
    host: &str,
) -> SharedString {
    match failure {
        ConnectionFailure::Unreachable => strings.format(
            "onboarding.desktopRecovery.remoteUnreachable.description",
            &[("host", host)],
        ),
        ConnectionFailure::NotOpenChamber => strings.format(
            "onboarding.desktopRecovery.incompatibleServer.description",
            &[("host", host)],
        ),
        ConnectionFailure::Incompatible => {
            strings.get("onboarding.remoteConnection.probe.incompatibleMessage")
        }
        ConnectionFailure::AuthenticationRequired => {
            strings.get("onboarding.remoteConnection.probe.authMessage")
        }
        ConnectionFailure::AuthenticationRejected => {
            strings.get("sessionAuth.error.incorrectPassword")
        }
    }
}
