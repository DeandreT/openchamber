//! The sidebar column: title row, toolbar, session list and footer.
//!
//! Geometry follows the web client's timeline sidebar
//! (`packages/ui/src/components/session/sidebar/`): rows keep a 262 px slot
//! inside the list's `ps-2.5 pe-2` padding while zone and project headers
//! paint across the full width, timeline rows carry no tree gutter, and the
//! gap that opens each section comes from the core row model.

use gpui_kit::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div, list,
    prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::grouping::GroupLabel;
use openchamber_native_core::relative_time::{self, AgeUnit};
use openchamber_native_core::session_index::IndexStatus;
use openchamber_native_core::sidebar_rows::{
    ActivityKey, EmptyKind, GroupLoadState, SectionSpacing, ShowControl,
};

use crate::activation::{on_activate, on_activate_split};
use crate::baseline::CssTextExt;
use crate::chrome::{self, ButtonTone, CONTROL_RADIUS, IconButton, TITLE_ROW_HEIGHT, glyph};
use crate::connection::{ConnectionFailure, ConnectionState};
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::sidebar_item::SidebarItem;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// Default sidebar width of the desktop layout.
pub const SIDEBAR_WIDTH: f32 = 280.0;
const TOOLBAR_HEIGHT: f32 = 40.0;
const FOOTER_HEIGHT: f32 = 40.0;
/// The list's inline padding (`ps-2.5 pe-2`); row slots span the rest.
const LIST_PADDING_START: f32 = 10.0;
const LIST_PADDING_END: f32 = 8.0;
const HEADER_HEIGHT: f32 = 28.0;
/// Zone and project headers: text after a 16 px inset, a 14 px glyph and a
/// 6 px gap.
const HEADER_INSET_START: f32 = 16.0;
const HEADER_GLYPH: f32 = 14.0;
const SESSION_HEIGHT: f32 = 32.0;
/// `my-px` around every session row.
const SESSION_MARGIN: f32 = 1.0;
/// Timeline rows have no gutter: their content sits 10 px in.
const TIMELINE_INSET: f32 = 10.0;
/// The provider logo before a compact row's age: `h-3 w-3 opacity-45`.
const ROW_LOGO: f32 = 12.0;
const ROW_LOGO_OPACITY: f32 = 0.45;
/// Tree geometry: 12 px per level from an 8 px edge, then the slot and a gap.
const TREE_BASE_PADDING: f32 = 6.0;
const TREE_INDENT: f32 = 12.0;
const TREE_SLOT: f32 = 14.0;
const TREE_GAP: f32 = 6.0;
/// The design system's type scale, in rems of the 16 px root.
const LABEL_SIZE: f32 = 0.843_75;
const LABEL_LINE: f32 = 1.25;
const HEADER_SIZE: f32 = 0.875;
const MICRO_SIZE: f32 = 0.75;
const XS_SIZE: f32 = 0.75;
const XS_LINE: f32 = 0.9375;

impl Workspace {
    pub(crate) fn render_sidebar(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let rows = list(
            self.sidebar_list.clone(),
            cx.processor(|this, index: usize, window, cx| {
                this.render_sidebar_row(index, window, cx)
            }),
        )
        .size_full();
        let status = self.connection_status(&strings);
        div()
            .id("sidebar-column")
            .relative()
            .w(px(SIDEBAR_WIDTH))
            .flex_shrink_0()
            .h_full()
            .bg(palette.muted)
            .flex()
            .flex_col()
            .child(
                chrome::drag_region("sidebar-title", cx)
                    .h(px(TITLE_ROW_HEIGHT))
                    .flex_shrink_0()
                    .flex_row_from(direction)
                    .items_center()
                    .padding_start(direction, px(12.0))
                    .child(self.render_title_controls(palette, direction, cx)),
            )
            .child(self.render_sidebar_toolbar(&strings, palette, direction, cx))
            .when_some(status, |this, status| {
                this.child(
                    div()
                        .id("connection-status")
                        .role(Role::Status)
                        .accessibility_id("connection-status")
                        .aria_label(status.clone())
                        .flex_shrink_0()
                        .padding_start(direction, px(LIST_PADDING_START + TIMELINE_INSET))
                        .padding_end(direction, px(LIST_PADDING_END))
                        .py_1()
                        .text_size(rems(XS_SIZE))
                        .text_color(palette.muted_foreground)
                        .child(status),
                )
            })
            .child(
                div()
                    .id("sidebar")
                    .role(Role::Tree)
                    .accessibility_id("sidebar")
                    .flex_1()
                    .min_h_0()
                    .padding_start(direction, px(LIST_PADDING_START))
                    .padding_end(direction, px(LIST_PADDING_END))
                    .pb_1()
                    .child(rows),
            )
            .child(self.render_sidebar_footer(&strings, palette, direction, cx))
            // The edge toward the chat, over the column rather than inside it,
            // so the rows keep their full slot.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .inset_end(direction, px(0.0))
                    .w(px(1.0))
                    .bg(palette.border),
            )
    }

    /// Menu, sidebar toggle and New session: the sidebar's title row, or the
    /// start of the header while the sidebar is closed.
    pub(crate) fn render_title_controls(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let strings = locale::strings(cx);
        let new_session_label = strings.get("sessions.sidebar.header.actions.newSession");
        let foreground = palette.foreground;
        let new_session = div()
            .id("new-session")
            .group("new-session")
            .role(Role::Button)
            .accessibility_id("new-session")
            .aria_label(new_session_label.clone())
            .h(px(24.0))
            .px(px(6.0))
            .margin_start(direction, px(-4.0))
            .rounded(px(CONTROL_RADIUS))
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .text_color(palette.muted_foreground)
            .hover(move |style| style.text_color(foreground))
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .child(
                glyph("chat-new", 16.0, palette.muted_foreground)
                    .group_hover("new-session", move |style| style.text_color(foreground)),
            )
            .child(
                div()
                    .css_text(LABEL_SIZE, LABEL_LINE, cx)
                    .child(new_session_label),
            );
        div()
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .child(
                IconButton::new(
                    "app-menu",
                    "menu-2",
                    strings.get("header.actions.openAppMenuAria"),
                )
                .render(palette, cx, |_, _, _| {}),
            )
            .child(
                IconButton::new(
                    "sidebar-toggle",
                    "layout-left",
                    strings.get("header.actions.openSessionsAria"),
                )
                .tone(if self.sidebar_open {
                    ButtonTone::Active
                } else {
                    ButtonTone::Quiet
                })
                .render(palette, cx, |this, _, cx| this.toggle_sidebar(cx)),
            )
            .child(
                on_activate(new_session, cx, |this, window, cx| {
                    this.show_new_session(window, cx);
                })
                .test_support(),
            )
    }

    fn render_sidebar_toolbar(
        &self,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let button = |id: &'static str, icon: &'static str, key: &str| {
            IconButton::new(id, icon, strings.get(key)).render(palette, cx, |_, _, _| {})
        };
        div()
            .id("sidebar-toolbar")
            .h(px(TOOLBAR_HEIGHT))
            .flex_shrink_0()
            .flex_row_from(direction)
            .items_center()
            .justify_between()
            .padding_start(direction, px(13.0))
            .padding_end(direction, px(10.0))
            .child(
                div()
                    .flex_row_from(direction)
                    .gap(px(6.0))
                    .child(button(
                        "source-board",
                        "git-pull-request",
                        "sourceBoard.title",
                    ))
                    .child(button(
                        "scheduled-tasks",
                        "calendar-schedule",
                        "sessions.sidebar.header.actions.scheduledTasks",
                    ))
                    .child(button("archive", "archive", "sessions.sidebar.nav.archive"))
                    .child(button(
                        "integrations",
                        "store-2",
                        "integrationsCatalog.sidebar.open",
                    )),
            )
            .child(
                div()
                    .flex_row_from(direction)
                    .gap(px(6.0))
                    .child(button(
                        "search-sessions",
                        "search",
                        "sessions.sidebar.header.actions.searchSessions",
                    ))
                    .child(button(
                        "add-project",
                        "folder-add",
                        "sessions.sidebar.header.actions.addProject",
                    ))
                    .child(button(
                        "display-mode",
                        "equalizer-2",
                        "sessions.sidebar.header.displayMode.label",
                    )),
            )
    }

    fn render_sidebar_footer(
        &self,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let button = |id: &'static str, icon: &'static str, key: &str| {
            IconButton::new(id, icon, strings.get(key))
                .tone(ButtonTone::QuietFilled)
                .render(palette, cx, |_, _, _| {})
        };
        let instance = self.instance_name(strings);
        let hover_fill = palette.hover.opacity(0.5);
        let foreground = palette.foreground;
        div()
            .id("sidebar-footer")
            .h(px(FOOTER_HEIGHT))
            .flex_shrink_0()
            .flex_row_from(direction)
            .items_center()
            .px(px(10.0))
            .child(
                div()
                    .flex_row_from(direction)
                    .gap(px(4.0))
                    .child(button(
                        "settings",
                        "settings-3",
                        "sessions.sidebar.footer.actions.settings",
                    ))
                    .child(button("usage-stats", "bar-chart", "usageStats.openAction"))
                    .child(button(
                        "shortcuts",
                        "command",
                        "sessions.sidebar.footer.actions.shortcuts",
                    ))
                    .child(button(
                        "about",
                        "information",
                        "sessions.sidebar.footer.actions.aboutOpenChamber",
                    )),
            )
            .child(div().flex_1())
            .child(
                on_activate(
                    div()
                        .id("instance-switcher")
                        .group("instance-switcher")
                        .role(Role::Button)
                        .accessibility_id("instance-switcher")
                        .aria_label(
                            strings.format(
                                "header.services.openWithCurrent",
                                &[("current", &instance)],
                            ),
                        )
                        .h(px(24.0))
                        .px(px(8.0))
                        .rounded(px(CONTROL_RADIUS))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(6.0))
                        .cursor_pointer()
                        .text_color(palette.muted_foreground)
                        .hover(move |style| style.bg(hover_fill).text_color(foreground))
                        .child(
                            glyph("server", 16.0, palette.muted_foreground)
                                .group_hover("instance-switcher", move |style| {
                                    style.text_color(foreground)
                                }),
                        )
                        .child(
                            div()
                                .css_text(LABEL_SIZE, LABEL_LINE, cx)
                                .child(instance.clone()),
                        ),
                    cx,
                    |_, _, _| {},
                )
                .test_support(),
            )
    }

    /// The name of the instance this window shows: Local for this machine.
    fn instance_name(&self, strings: &Strings) -> SharedString {
        match &self.connection {
            Some(connection) if !connection.config.endpoint.is_loopback() => {
                connection.config.endpoint.authority().into()
            }
            _ => strings.get("desktopHostSwitcher.instance.local"),
        }
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
        let spacing = match self.sidebar_spacing.get(index).copied().flatten() {
            Some(SectionSpacing::Section) => 8.0,
            Some(SectionSpacing::Group) => 6.0,
            None => 0.0,
        };
        let palette = cx.global::<Palette>().clone();
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let row = match &item {
            SidebarItem::Zone {
                key,
                activity,
                collapsed,
            } => {
                let icon = match activity {
                    ActivityKey::Chats => "chat-4",
                    ActivityKey::Pinned => "pushpin",
                    ActivityKey::Work => "eye",
                    ActivityKey::Timeline => "folder",
                };
                let toggled = item.clone();
                self.header_row(
                    HeaderRow {
                        key,
                        label: zone_label(&strings, *activity),
                        icon,
                        collapsed: *collapsed,
                        expandable: true,
                        spacing,
                    },
                    &palette,
                    direction,
                    cx,
                    move |this, _, cx| this.toggle_collapsed(&toggled, cx),
                )
            }
            SidebarItem::Project {
                key,
                label,
                collapsed,
                ..
            } => {
                let toggled = item.clone();
                self.header_row(
                    HeaderRow {
                        key,
                        label: label.clone(),
                        icon: "folder",
                        collapsed: *collapsed,
                        expandable: false,
                        spacing,
                    },
                    &palette,
                    direction,
                    cx,
                    move |this, _, cx| this.toggle_collapsed(&toggled, cx),
                )
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
                    chevron(*collapsed, direction, &palette).into_any_element()
                };
                let row = tree_row(key, &palette, direction, 1, slot, false)
                    .role(Role::Button)
                    .aria_label(text.clone())
                    .aria_expanded(!*collapsed)
                    .h(px(SESSION_HEIGHT))
                    .child(glyph(icon, 16.0, palette.muted_foreground))
                    .child(row_text(text, palette.muted_foreground, cx));
                let toggled = item.clone();
                on_activate(row, cx, move |this, _, cx| {
                    this.toggle_collapsed(&toggled, cx)
                })
                .test_support()
                .into_any_element()
            }
            SidebarItem::Worktree {
                key,
                label,
                expanded,
                session_ids,
                ..
            } => {
                let (text, icon) = group_label(&strings, label);
                let selected = self
                    .selected
                    .as_ref()
                    .is_some_and(|id| session_ids.contains(id));
                let toggle = item.clone();
                let slot = if session_ids.is_empty() {
                    div().into_any_element()
                } else {
                    on_activate(
                        div()
                            .id(SharedString::from(format!("{key}:chevron")))
                            .role(Role::Button)
                            .aria_label(text.clone())
                            .aria_expanded(*expanded)
                            .child(chevron(!*expanded, direction, &palette)),
                        cx,
                        move |this, _, cx| {
                            cx.stop_propagation();
                            this.toggle_collapsed(&toggle, cx);
                        },
                    )
                    .test_support()
                    .into_any_element()
                };
                let row = tree_row(key, &palette, direction, 0, slot, selected)
                    .role(Role::TreeItem)
                    .aria_label(text.clone())
                    .aria_expanded(*expanded)
                    .aria_selected(selected)
                    .h(px(SESSION_HEIGHT))
                    .child(glyph(icon, 14.0, palette.muted_foreground))
                    .child(row_text(text, palette.muted_foreground, cx));
                if let Some(id) = session_ids.first().cloned() {
                    on_activate(row, cx, move |this, window, cx| {
                        this.select_session(id.clone(), window, cx)
                    })
                    .test_support()
                    .into_any_element()
                } else {
                    row.test_support().into_any_element()
                }
            }
            SidebarItem::Session { context, .. } if !context.has_tree_slot() => {
                self.timeline_session_row(&item, spacing, &palette, direction, cx)
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
                        .child(chevron(!*expanded, direction, &palette));
                    on_activate(button, cx, move |this, _, cx| {
                        cx.stop_propagation();
                        this.toggle_expanded(&expansion_key, cx);
                    })
                    .test_support()
                    .into_any_element()
                } else {
                    div().into_any_element()
                };
                let title = display_title(&strings, title);
                let text_color = if selected {
                    palette.selection_foreground
                } else if streaming {
                    palette.foreground
                } else {
                    palette.muted_foreground
                };
                let node = div()
                    .id(key.clone())
                    .accessibility_id(key.clone())
                    .role(Role::TreeItem)
                    .aria_label(title.clone())
                    .aria_selected(selected)
                    .aria_level(depth + 1)
                    .when(*has_children, |this| this.aria_expanded(*expanded))
                    .pt(px(SESSION_MARGIN))
                    .pb(px(SESSION_MARGIN + spacing));
                let target_key = SharedString::from(format!("{key}:target"));
                let row = tree_row(&target_key, &palette, direction, *depth, slot, selected)
                    .h(px(SESSION_HEIGHT))
                    .child(row_text(title, text_color, cx))
                    .when(streaming, |this| {
                        this.child(glyph("circle", 16.0, palette.info))
                    })
                    .when(*pinned && !streaming, |this| {
                        this.child(glyph("pushpin", 16.0, palette.muted_foreground))
                    });
                let (node, row) = on_activate_split(node, row, cx, move |this, window, cx| {
                    this.select_session(id.clone(), window, cx);
                });
                node.child(row.test_support())
                    .test_support()
                    .into_any_element()
            }
            SidebarItem::Show {
                key,
                control,
                depth,
                flat,
                ..
            } => {
                let label = match control {
                    ShowControl::More => strings.get("sessions.sidebar.group.showMore"),
                    ShowControl::Fewer => strings.get("sessions.sidebar.group.showFewer"),
                };
                let inset = if *flat {
                    TIMELINE_INSET
                } else {
                    tree_text_inset(*depth)
                };
                let foreground = palette.foreground;
                let row = div()
                    .id(key.clone())
                    .accessibility_id(key.clone())
                    .role(Role::Button)
                    .aria_label(label.clone())
                    .mt(px(2.0))
                    .py(px(2.0))
                    .padding_start(direction, px(inset))
                    .flex_row_from(direction)
                    .cursor_pointer()
                    .css_text(XS_SIZE, XS_LINE, cx)
                    .text_color(palette.muted_foreground.opacity(0.7))
                    .hover(move |style| style.text_color(foreground).underline())
                    .child(label);
                let revealed = item.clone();
                on_activate(row, cx, move |this, _, cx| this.reveal(&revealed, cx))
                    .test_support()
                    .into_any_element()
            }
            SidebarItem::Status {
                key,
                state,
                depth,
                flat,
            } => {
                let label = status_label(&strings, *state);
                let color = match state {
                    GroupLoadState::Loading | GroupLoadState::Ready => palette.muted_foreground,
                    _ => palette.error,
                };
                let inset = if *flat {
                    TIMELINE_INSET
                } else {
                    tree_text_inset(*depth)
                };
                notice_row(key, direction, inset, label, color).into_any_element()
            }
            SidebarItem::Empty { key, kind, depth } => match kind {
                EmptyKind::Archived => notice_row(
                    key,
                    direction,
                    tree_text_inset(*depth),
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
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(rems(XS_SIZE))
                            .text_color(palette.muted_foreground)
                            .child(strings.get("sessions.sidebar.empty.noSessions.description")),
                    )
                    .into_any_element(),
            },
        };
        match item {
            SidebarItem::Zone { .. } | SidebarItem::Project { .. } => row,
            SidebarItem::Session { .. } => row,
            _ => div().pb(px(spacing)).child(row).into_any_element(),
        }
    }

    /// A zone or project header: its accessible node keeps the row's slot
    /// while its fill and pointer target span the sidebar.
    fn header_row(
        &self,
        header: HeaderRow<'_>,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
        action: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> AnyElement {
        let group: SharedString = format!("{}:hover", header.key).into();
        let icon_color = palette.muted_foreground.opacity(0.8);
        let node = div()
            .id(header.key.clone())
            .role(Role::Button)
            .accessibility_id(header.key.clone())
            .aria_label(header.label.clone())
            .when(header.expandable, |this| {
                this.aria_expanded(!header.collapsed)
            })
            .pb(px(header.spacing));
        let target = div()
            .id(SharedString::from(format!("{}:target", header.key)))
            .group(group.clone())
            .h(px(HEADER_HEIGHT))
            .margin_start(direction, px(-LIST_PADDING_START))
            .margin_end(direction, px(-LIST_PADDING_END))
            .bg(palette.muted)
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .padding_start(direction, px(HEADER_INSET_START))
            .padding_end(direction, px(14.0))
            .cursor_pointer()
            .child(
                div()
                    .size(px(HEADER_GLYPH))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        glyph(header.icon, HEADER_GLYPH, icon_color)
                            .group_hover(group.clone(), |style| style.invisible()),
                    )
                    .child(
                        div()
                            .absolute()
                            .invisible()
                            .group_hover(group, |style| style.visible())
                            .child(chevron(header.collapsed, direction, palette)),
                    ),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .css_text(HEADER_SIZE, LABEL_LINE, cx)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(palette.foreground)
                    .child(header.label),
            );
        let (node, target) = on_activate_split(node, target, cx, action);
        node.child(target.test_support())
            .test_support()
            .into_any_element()
    }

    /// A one-line row of the timeline, Chats or the mixed zones: no gutter,
    /// the project's glyph where projects mix, the title and the age.
    fn timeline_session_row(
        &self,
        item: &SidebarItem,
        spacing: f32,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let SidebarItem::Session {
            key,
            session_id,
            title,
            activity_ms,
            project_label,
            provider_id,
            ..
        } = item
        else {
            return div().into_any_element();
        };
        let logo = provider_id.as_ref().and_then(|provider_id| {
            self.render_provider_logo(provider_id, ROW_LOGO, ROW_LOGO_OPACITY, palette)
        });
        let strings = locale::strings(cx);
        let selected = self.selected.as_ref() == Some(session_id);
        let streaming = self.is_streaming(session_id);
        let title = display_title(strings, title);
        let title_color = if selected {
            palette.selection_foreground
        } else if streaming {
            palette.foreground
        } else {
            palette.muted_foreground
        };
        let age = compact_age(strings, *activity_ms);
        let hover_fill = palette.hover.opacity(0.6);
        let node = div()
            .id(key.clone())
            .role(Role::TreeItem)
            .accessibility_id(key.clone())
            .aria_label(title.clone())
            .aria_selected(selected)
            .pt(px(SESSION_MARGIN))
            .pb(px(SESSION_MARGIN + spacing));
        let target = div()
            .id(SharedString::from(format!("{key}:target")))
            .h(px(SESSION_HEIGHT))
            .rounded(px(CONTROL_RADIUS))
            .flex_row_from(direction)
            .items_center()
            .gap(px(4.0))
            .px(px(TIMELINE_INSET))
            .cursor_pointer()
            .when(selected, |this| this.bg(palette.selection.opacity(0.7)))
            .when(!selected, |this| {
                this.hover(move |style| style.bg(hover_fill))
            })
            .when_some(project_label.clone(), |this, _| {
                this.child(glyph(
                    "folder",
                    12.0,
                    palette.muted_foreground.opacity(0.75),
                ))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .css_text(LABEL_SIZE, LABEL_LINE, cx)
                    .text_color(title_color)
                    .child(title),
            )
            .children(logo)
            .child(
                div()
                    .flex_shrink_0()
                    .css_text(MICRO_SIZE, MICRO_SIZE, cx)
                    .text_color(palette.muted_foreground.opacity(0.5))
                    .child(age),
            );
        let id = session_id.clone();
        let (node, target) = on_activate_split(node, target, cx, move |this, window, cx| {
            this.select_session(id.clone(), window, cx);
        });
        node.child(target.test_support())
            .test_support()
            .into_any_element()
    }
}

/// What a zone or project header shows.
struct HeaderRow<'a> {
    key: &'a SharedString,
    label: SharedString,
    icon: &'static str,
    collapsed: bool,
    /// Zones report their disclosure state; project headers do not.
    expandable: bool,
    /// The gap below that opens the next section.
    spacing: f32,
}

fn display_title(strings: &Strings, title: &SharedString) -> SharedString {
    if title.is_empty() {
        strings.get("sessions.sidebar.session.untitled")
    } else {
        title.clone()
    }
}

/// The compact age of a session's last conversation, such as `12m`.
fn compact_age(strings: &Strings, activity_ms: i64) -> SharedString {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        });
    let age = relative_time::compact_age(now_ms, activity_ms);
    let key = match age.unit {
        AgeUnit::Minutes => "common.relative.minutesAgoCompact",
        AgeUnit::Hours => "common.relative.hoursAgoCompact",
        AgeUnit::Days => "common.relative.daysAgoCompact",
        AgeUnit::Weeks => "common.relative.weeksAgoCompact",
        AgeUnit::Months => "common.relative.monthsAgoCompact",
        AgeUnit::Years => "common.relative.yearsAgoCompact",
    };
    strings.format(key, &[("count", &age.count.to_string())])
}

fn zone_label(strings: &Strings, activity: ActivityKey) -> SharedString {
    match activity {
        ActivityKey::Chats => strings.get("sessions.sidebar.activity.chatsTitle"),
        ActivityKey::Pinned => strings.get("sessions.sidebar.activity.pinnedTitle"),
        ActivityKey::Work => strings.get("sessions.sidebar.work.title"),
        ActivityKey::Timeline => strings.get("sessions.sidebar.activity.projectsTitle"),
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

/// A disclosure chevron; collapsed rows point toward the end side.
pub(crate) fn chevron(
    collapsed: bool,
    direction: Direction,
    palette: &Palette,
) -> impl IntoElement {
    let name = match (collapsed, direction) {
        (false, _) => "arrow-down-s",
        (true, Direction::LeftToRight) => "arrow-right-s",
        (true, Direction::RightToLeft) => "arrow-left-s",
    };
    glyph(name, 14.0, palette.muted_foreground)
}

/// Where tree text starts at `depth`: past the level's slot and gap.
fn tree_text_inset(depth: usize) -> f32 {
    TREE_BASE_PADDING + depth as f32 * TREE_INDENT + TREE_SLOT + TREE_GAP
}

/// A tree row with its chevron slot; `slot` is empty for leaf rows.
fn tree_row(
    key: &SharedString,
    palette: &Palette,
    direction: Direction,
    depth: usize,
    slot: AnyElement,
    selected: bool,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let padding = TREE_BASE_PADDING + depth as f32 * TREE_INDENT;
    let hover_fill = palette.hover.opacity(0.6);
    div()
        .id(key.clone())
        .accessibility_id(key.clone())
        .rounded(px(CONTROL_RADIUS))
        .padding_start(direction, px(padding))
        .padding_end(direction, px(TIMELINE_INSET))
        .flex_row_from(direction)
        .items_center()
        .gap(px(TREE_GAP))
        .cursor_pointer()
        .when(selected, |this| this.bg(palette.selection.opacity(0.7)))
        .when(!selected, |this| {
            this.hover(move |style| style.bg(hover_fill))
        })
        .child(
            div()
                .w(px(TREE_SLOT))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .child(slot),
        )
}

fn notice_row(
    key: &SharedString,
    direction: Direction,
    inset: f32,
    label: SharedString,
    color: Hsla,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(key.clone())
        .accessibility_id(key.clone())
        .py_1()
        .padding_start(direction, px(inset))
        .flex_row_from(direction)
        .text_size(rems(MICRO_SIZE))
        .text_color(color)
        .child(label)
}

fn row_text(text: SharedString, color: Hsla, cx: &gpui_kit::App) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .truncate()
        .css_text(LABEL_SIZE, LABEL_LINE, cx)
        .text_color(color)
        .child(text)
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
