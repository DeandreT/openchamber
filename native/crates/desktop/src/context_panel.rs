//! The work status panel beside the chat and the panel rail at the end edge.
//!
//! Mirrors `packages/ui/src/components/layout/ContextPanelRail.tsx`, the rail
//! surfaces of `packages/ui/src/lib/surfaces/registry.ts` and the docked work
//! status card (`.oc-panel-edge`).

use gpui_kit::{
    Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Role,
    StatefulInteractiveElement, Styled, TestSupportExt, div, px, rems,
};

use crate::activation::on_activate;
use crate::chrome::{ButtonTone, CONTROL_RADIUS, IconButton};
use crate::elevation;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// Width of the docked work status panel.
pub const PANEL_WIDTH: f32 = 324.0;
/// Width of the rail.
pub const RAIL_WIDTH: f32 = 44.0;
const CARD_RADIUS: f32 = 12.0;
const RAIL_BUTTON: f32 = 28.0;
const RAIL_GLYPH: f32 = 18.0;
/// `text-xs` of the design system, in rems of the 16 px root.
const XS_SIZE: f32 = 0.75;
const XS_LINE: f32 = 1.0;

/// The rail's surfaces in their default order: ID, glyph and label key.
const RAIL_SURFACES: &[(&str, &str, &str)] = &[
    ("rail-git", "git-branch", "layout.rightSidebar.git"),
    ("rail-diff", "layout-column", "contextPanel.mode.diff"),
    ("rail-walkthrough", "route", "contextPanel.mode.walkthrough"),
    ("rail-files", "file-edit", "contextPanel.mode.files"),
    ("rail-terminal", "terminal-box", "layout.mainTab.terminal"),
    ("rail-notes", "book-marked", "contextRail.surface.notes"),
    ("rail-browser", "global", "contextPanel.mode.browser"),
];

impl Workspace {
    /// The docked work status card.
    pub(crate) fn render_work_status_panel(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let strings = locale::strings(cx);
        div()
            .id("work-status-panel")
            .w(px(PANEL_WIDTH))
            .flex_shrink_0()
            .h_full()
            .bg(palette.background)
            .child(
                div()
                    .id("work-status")
                    .role(Role::Complementary)
                    .accessibility_id("work-status")
                    .aria_label(strings.get("chat.workStatus.ariaLabel"))
                    .relative()
                    .my(px(16.0))
                    .margin_start(direction, px(8.0))
                    .margin_end(direction, px(16.0))
                    .rounded(px(CARD_RADIUS))
                    .bg(elevation::panel_fill(palette))
                    .shadow(elevation::panel_edge(palette))
                    .p(px(8.0))
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .mb(px(2.0))
                            .px(px(4.0))
                            .padding_end(direction, px(28.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(rems(XS_SIZE))
                                    .line_height(rems(XS_LINE))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(palette.foreground)
                                    .child(strings.get("chat.workStatus.section.session")),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(6.0))
                            .inset_end(direction, px(8.0))
                            .child(
                                IconButton::new(
                                    "work-status-sections",
                                    "equalizer-2",
                                    strings.get("chat.workStatus.sections.open"),
                                )
                                .size(20.0, 16.0)
                                .render(palette, cx, |_, _, _| {}),
                            ),
                    )
                    // The ring sits above the content, as `::after` does.
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .rounded(px(CARD_RADIUS))
                            .shadow(elevation::panel_edge_ring(palette)),
                    ),
            )
    }

    /// The column of panel surfaces at the end edge.
    pub(crate) fn render_rail(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let strings = locale::strings(cx);
        let tint = |color: gpui_kit::Hsla| {
            div()
                .absolute()
                .top(px(RAIL_GLYPH * 0.2))
                .h(px(RAIL_GLYPH * 0.6))
                .w(px(RAIL_GLYPH * 0.25))
                .bg(color.opacity(0.25))
        };
        let foreground = palette.foreground;
        let context = on_activate(
            div()
                .id("rail-context")
                .group("rail-context")
                .role(Role::Button)
                .accessibility_id("rail-context")
                .aria_label(strings.get("contextPanel.mode.context"))
                .size(px(RAIL_BUTTON))
                .rounded(px(CONTROL_RADIUS))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                // The context usage ring, empty until a session reports usage.
                .child(
                    div()
                        .size(px(RAIL_GLYPH))
                        .rounded_full()
                        .border_2()
                        .border_color(palette.muted_foreground.opacity(0.3))
                        .group_hover("rail-context", move |style| {
                            style.border_color(foreground.opacity(0.3))
                        }),
                ),
            cx,
            |_, _, _| {},
        )
        .test_support();
        div()
            .id("rail")
            .role(Role::Navigation)
            .accessibility_id("rail")
            .aria_label(strings.get("contextRail.aria.rail"))
            .w(px(RAIL_WIDTH))
            .flex_shrink_0()
            .h_full()
            .bg(palette.background)
            .py(px(8.0))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.0))
            .child(context)
            .children(RAIL_SURFACES.iter().map(|(id, icon, key)| {
                let button = IconButton::new(*id, icon, strings.get(key))
                    .size(RAIL_BUTTON, RAIL_GLYPH)
                    .render(palette, cx, |_, _, _| {});
                if *id != "rail-diff" {
                    return button;
                }
                // The changes surface tints its start column removed and its
                // end column added.
                let (start, end) = match direction {
                    Direction::LeftToRight => (palette.status_error, palette.status_success),
                    Direction::RightToLeft => (palette.status_success, palette.status_error),
                };
                div()
                    .relative()
                    .child(button)
                    .child(
                        div()
                            .absolute()
                            .top(px((RAIL_BUTTON - RAIL_GLYPH) / 2.0))
                            .left(px((RAIL_BUTTON - RAIL_GLYPH) / 2.0))
                            .size(px(RAIL_GLYPH))
                            .rounded(px(2.0))
                            .overflow_hidden()
                            .child(tint(start).left(px(RAIL_GLYPH * 0.2)))
                            .child(tint(end).right(px(RAIL_GLYPH * 0.2))),
                    )
                    .into_any_element()
            }))
            .child(
                IconButton::new(
                    "rail-configure",
                    "equalizer-2",
                    strings.get("contextRail.configure.open"),
                )
                .size(RAIL_BUTTON, RAIL_GLYPH)
                .tone(ButtonTone::Dim)
                .render(palette, cx, |_, _, _| {}),
            )
    }
}
