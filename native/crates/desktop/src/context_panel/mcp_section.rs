//! The work status card's MCP section (`WorkStatusMcpSection.tsx`): the MCP
//! servers of the directory being worked in, by name, each with its on/off
//! switch. A server asking for sign-in or reporting a failure shows that
//! state as the control to press.
//!
//! The section is absent while no server is known: before the first read,
//! after a failed first read and for a directory without servers alike.

use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Toggled, div, prelude::FluentBuilder, px,
};
use openchamber_native_transport::mcp::{McpConnection, McpServerStatus};

use crate::activation::on_activate;
use crate::assets::MCP_ICON;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::context_panel::{
    ActionTone, ROW_HEIGHT, ROW_LINE, ROW_SIZE, Section, collapsible_section, row_action,
    section_expanded, toggle_section,
};
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The section's key in the stored fold states (`workStatusExpandedSections`).
const SECTION_ID: &str = "mcp";

/// The switch's layout box (`h-5 w-9`) and what `scale-75` draws inside it.
const SWITCH_BOX: (f32, f32) = (36.0, 20.0);
const SWITCH_TRACK: (f32, f32) = (27.0, 15.0);
const SWITCH_PADDING: f32 = 1.5;
const SWITCH_THUMB: f32 = 12.0;
/// The busy mark inside the thumb (`size-3`, scaled).
const SWITCH_SPINNER: f32 = 9.0;

/// The state a row shows as its trailing control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StateAction {
    /// The server waits for a sign-in; a press says where to sign in.
    NeedsAuth,
    /// The server failed; a press tries to connect it again.
    Failed,
}

/// The trailing control `connection` asks for, if any.
pub(crate) fn state_action(connection: McpConnection) -> Option<StateAction> {
    match connection {
        McpConnection::NeedsAuth => Some(StateAction::NeedsAuth),
        McpConnection::Failed => Some(StateAction::Failed),
        McpConnection::Connected
        | McpConnection::Pending
        | McpConnection::Disabled
        | McpConnection::Unrecognized => None,
    }
}

/// The heading's aggregate: connected servers over all servers.
pub(crate) fn summary(servers: &[McpServerStatus]) -> String {
    let connected = servers
        .iter()
        .filter(|server| server.connection == McpConnection::Connected)
        .count();
    format!("{connected}/{}", servers.len())
}

impl Workspace {
    /// The section, once the directory being worked in has at least one
    /// server.
    pub(crate) fn render_mcp_section(
        &self,
        palette: &Palette,
        direction: Direction,
        first_heading: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let directory = self.mcp_directory()?;
        let servers = self.mcp_status.store.servers(&directory);
        if servers.is_empty() {
            return None;
        }
        let strings = locale::strings(cx);
        let rows = servers
            .iter()
            .map(|server| {
                let busy = self.mcp_status.is_busy(&directory, &server.name);
                server_row(server, busy, strings, palette, direction, cx)
            })
            .collect();
        Some(collapsible_section(
            Section {
                id: "work-status-mcp",
                icon: MCP_ICON,
                title: strings.get("chat.workStatus.section.mcp"),
                summary: Some(summary(servers).into()),
                // Folded until the user unfolds it, as `defaultExpanded` is off.
                expanded: section_expanded(SECTION_ID, false, cx),
                toggle: |_, cx| toggle_section(SECTION_ID, false, cx),
            },
            rows,
            palette,
            direction,
            first_heading,
            cx,
        ))
    }
}

/// One server's 28 px row: its switch, its name and the state it asks the
/// user to act on.
fn server_row(
    server: &McpServerStatus,
    busy: bool,
    strings: &Strings,
    palette: &Palette,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    let connected = server.connection == McpConnection::Connected;
    let action = state_action(server.connection).map(|action| {
        let name = server.name.clone();
        match action {
            StateAction::NeedsAuth => row_action(
                format!("work-status-mcp-sign-in-{}", server.name),
                strings.get("chat.workStatus.mcp.needsAuth"),
                ActionTone::Warning,
                busy,
                palette,
                cx,
                |this, window, cx| this.report_mcp_needs_auth(window, cx),
            ),
            StateAction::Failed => row_action(
                format!("work-status-mcp-failed-{}", server.name),
                strings.get("chat.workStatus.mcp.failed"),
                ActionTone::Error,
                busy,
                palette,
                cx,
                move |this, window, cx| this.set_mcp_server_enabled(&name, true, window, cx),
            ),
        }
    });
    div()
        .h(px(ROW_HEIGHT))
        .px(px(4.0))
        .flex_row_from(direction)
        .items_center()
        .gap(px(8.0))
        .text_color(palette.muted_foreground)
        .child(switch(
            server, connected, busy, strings, palette, direction, cx,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .css_text(ROW_SIZE, ROW_LINE, cx)
                .child(SharedString::from(server.name.clone())),
        )
        .children(action)
        .into_any_element()
}

/// The server's switch: on while connected, tinted as status while on, and
/// held with a busy thumb while its change is in flight.
fn switch(
    server: &McpServerStatus,
    connected: bool,
    busy: bool,
    strings: &Strings,
    palette: &Palette,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    let id = SharedString::from(format!("work-status-mcp-toggle-{}", server.name));
    let thumb = div()
        .size(px(SWITCH_THUMB))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(if busy {
            palette.status_warning
        } else if connected {
            palette.primary_foreground
        } else {
            palette.muted_foreground
        })
        .when(busy, |thumb| {
            thumb.child(glyph("loader-4", SWITCH_SPINNER, palette.background))
        });
    let track = div()
        .w(px(SWITCH_TRACK.0))
        .h(px(SWITCH_TRACK.1))
        .p(px(SWITCH_PADDING))
        .rounded_full()
        .bg(if connected {
            palette.status_info
        } else {
            palette.border
        })
        // The thumb rests at the start and moves to the end while on.
        .flex_row_from(direction)
        .items_center()
        .when(connected, |track| track.justify_end())
        .child(thumb);
    let control = div()
        .id(id.clone())
        .role(Role::Switch)
        .accessibility_id(id)
        .aria_label(strings.format("chat.workStatus.mcp.toggle", &[("name", &server.name)]))
        .aria_toggled(if connected {
            Toggled::True
        } else {
            Toggled::False
        })
        .w(px(SWITCH_BOX.0))
        .h(px(SWITCH_BOX.1))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .child(track);
    if busy {
        return control
            .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
            .test_support()
            .into_any_element();
    }
    let name = server.name.clone();
    on_activate(control.cursor_pointer(), cx, move |this, window, cx| {
        this.set_mcp_server_enabled(&name, !connected, window, cx)
    })
    .test_support()
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext};
    use serde_json::json;

    use crate::mcp_status::tests::connect;
    use crate::settings_test_server::{Server, settle};

    use super::*;

    fn server(name: &str, connection: McpConnection) -> McpServerStatus {
        McpServerStatus {
            name: name.to_owned(),
            connection,
        }
    }

    #[test]
    fn only_sign_in_and_failure_become_the_rows_control() {
        assert_eq!(
            state_action(McpConnection::NeedsAuth),
            Some(StateAction::NeedsAuth)
        );
        assert_eq!(
            state_action(McpConnection::Failed),
            Some(StateAction::Failed)
        );
        for quiet in [
            McpConnection::Connected,
            McpConnection::Pending,
            McpConnection::Disabled,
            McpConnection::Unrecognized,
        ] {
            assert_eq!(state_action(quiet), None);
        }
    }

    #[test]
    fn the_heading_counts_connected_servers_over_all_servers() {
        assert_eq!(
            summary(&[
                server("a", McpConnection::Connected),
                server("b", McpConnection::Failed),
                server("c", McpConnection::Pending),
            ]),
            "1/3"
        );
        assert_eq!(summary(&[server("a", McpConnection::Disabled)]), "0/1");
    }

    fn rendered(
        workspace: &Entity<Workspace>,
        window: AnyWindowHandle,
        cx: &mut TestAppContext,
    ) -> bool {
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                let palette = cx.global::<Palette>().clone();
                workspace
                    .render_mcp_section(&palette, Direction::LeftToRight, false, cx)
                    .is_some()
            })
        })
        .expect("test window")
    }

    #[gpui_kit::test]
    fn the_section_shows_once_servers_are_read(cx: &mut TestAppContext) {
        let populated = Server::new(|request| {
            if request.is("GET", "/api/mcp") {
                return (
                    200,
                    json!({"location": {}, "data": [
                        {"name": "docs", "status": {"status": "connected"}},
                        {"name": "auth", "status": {"status": "needs_auth", "error": "sign in"}},
                    ]}),
                );
            }
            (404, json!({"error": "unknown route"}))
        });
        let (window, workspace, directory) = connect(&populated, cx);
        assert!(
            !rendered(&workspace, window, cx),
            "nothing is known before the read"
        );
        settle(window, cx);
        assert!(rendered(&workspace, window, cx));
        cx.read(|cx| {
            let servers = workspace
                .read(cx)
                .mcp_status
                .store
                .servers(&directory)
                .to_vec();
            assert_eq!(summary(&servers), "1/2");
            assert_eq!(servers[0].name, "auth", "servers list by name");
        });
    }

    #[gpui_kit::test]
    fn a_directory_without_servers_shows_no_section(cx: &mut TestAppContext) {
        let empty = Server::new(|request| {
            if request.is("GET", "/api/mcp") {
                (200, json!({"location": {}, "data": []}))
            } else {
                (404, json!({"error": "unknown route"}))
            }
        });
        let (window, workspace, directory) = connect(&empty, cx);
        settle(window, cx);
        assert!(!rendered(&workspace, window, cx));
        cx.read(|cx| {
            assert!(
                workspace.read(cx).mcp_status.store.is_held(&directory),
                "the empty answer was read"
            );
        });
    }

    #[gpui_kit::test]
    fn a_failed_first_read_stays_silent(cx: &mut TestAppContext) {
        let failing = Server::new(|_| (503, json!({"error": "starting"})));
        let (window, workspace, directory) = connect(&failing, cx);
        settle(window, cx);
        assert!(!rendered(&workspace, window, cx));
        cx.read(|cx| {
            let status = &workspace.read(cx).mcp_status.store;
            assert!(!status.is_held(&directory));
            assert!(status.last_refresh_failed(&directory));
        });
    }
}
