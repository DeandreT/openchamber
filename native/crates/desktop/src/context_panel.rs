//! The work status panel beside the chat and the panel rail at the end edge.
//!
//! Mirrors `packages/ui/src/components/layout/ContextPanelRail.tsx`, the rail
//! surfaces of `packages/ui/src/lib/surfaces/registry.ts` and the docked work
//! status card (`.oc-panel-edge`).

use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, div, prelude::FluentBuilder,
    px,
};

use openchamber_native_core::context_usage::{self, ContextUsage, UsageTone};
use openchamber_native_core::send_config::ModelChoice;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{ButtonTone, CONTROL_RADIUS, IconButton, glyph};
use crate::elevation;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::send_catalog::ModelControl;
use crate::sidebar_view;
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
pub(crate) const XS_SIZE: f32 = 0.75;
pub(crate) const XS_LINE: f32 = 1.0;
/// A row's 13 px text, in rems of the 16 px root, on the base `leading-normal`
/// line of 1.5 times its size.
pub(crate) const ROW_SIZE: f32 = 0.8125;
pub(crate) const ROW_LINE: f32 = 1.218_75;
/// The compacted caption's `text-[11px] leading-4`.
const CAPTION_SIZE: f32 = 0.6875;
const CAPTION_LINE: f32 = 1.0;
/// The fill shown after a compaction, until a response reports tokens.
const UNKNOWN_PERCENT: &str = "\u{2014}";
/// Space above every section after the first (`mt-5`).
pub(crate) const SECTION_GAP: f32 = 20.0;
const HEADING_HEIGHT: f32 = 24.0;
pub(crate) const ROW_HEIGHT: f32 = 28.0;

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
                    // A flex column, as the web's sections are flex
                    // containers: no margin collapses between them.
                    .flex()
                    .flex_col()
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
                                    .css_text(XS_SIZE, XS_LINE, cx)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(palette.foreground)
                                    .child(strings.get("chat.workStatus.section.session")),
                            ),
                    )
                    .children(self.render_context_usage(palette, direction, cx))
                    .children(self.render_project_section(palette, direction, cx))
                    .children(self.render_turn_stats(palette, direction, cx))
                    .children(self.render_context_sources(palette, direction, cx))
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

    /// The selected session's context window: a row with its fill and a meter
    /// under it (`WorkStatusPrimaryGroup.tsx`), absent before any response
    /// reported tokens.
    fn render_context_usage(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let usage = self.session_context_usage()?;
        let strings = locale::strings(cx);
        let (value, percent, compacted) = match usage {
            ContextUsage::Measured { percent, .. } => {
                (context_usage::format_percent(percent), percent, false)
            }
            ContextUsage::Compacted { .. } => (UNKNOWN_PERCENT.to_owned(), 0.0, true),
        };
        let fill = match UsageTone::of(percent) {
            UsageTone::Safe => palette.status_success,
            UsageTone::Warn => palette.status_warning,
            UsageTone::Critical => palette.status_error,
        };
        let row = div()
            .id("work-status-context-usage")
            .role(Role::Group)
            .accessibility_id("work-status-context-usage")
            .aria_label(strings.get("chat.workStatus.action.openContext"))
            .h(px(ROW_HEIGHT))
            .px(px(4.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .text_color(palette.muted_foreground)
            .child(glyph("donut-chart", 16.0, palette.muted_foreground))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .css_text(ROW_SIZE, ROW_LINE, cx)
                    .child(strings.get("chat.workStatus.context.label")),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .css_text(ROW_SIZE, ROW_LINE, cx)
                    .child(SharedString::from(value)),
            );
        let meter = div()
            .mx(px(4.0))
            .mb(px(4.0))
            .h(px(4.0))
            .rounded_full()
            .overflow_hidden()
            .bg(palette.chat_divider)
            .flex_row_from(direction)
            .child(
                div()
                    .h_full()
                    .rounded_full()
                    .w(gpui_kit::relative((percent / 100.0).clamp(0.0, 1.0) as f32))
                    .bg(fill),
            );
        Some(
            div()
                .flex()
                .flex_col()
                .child(row)
                .child(meter)
                .when(compacted, |this| {
                    this.child(
                        div()
                            .mx(px(4.0))
                            .mb(px(4.0))
                            .css_text(CAPTION_SIZE, CAPTION_LINE, cx)
                            .text_color(palette.muted_foreground)
                            .child(strings.get("contextUsage.compacted.description")),
                    )
                })
                .into_any_element(),
        )
    }

    /// The fill of the selected session's window, measured against the
    /// context limit `useContextWindowLimits` picks: the record's model, then
    /// the newest answer's, then the composer's.
    fn session_context_usage(&self) -> Option<ContextUsage> {
        let session = self.selected_session()?;
        let open = self.open_sessions.get(&session.id)?;
        let directory = self.config_directory()?;
        let providers = self
            .send_catalog
            .directory(&directory)
            .and_then(|entry| entry.providers.ready())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let limit_of = |choice: &ModelChoice| {
            providers
                .iter()
                .find(|provider| provider.id == choice.provider_id)?
                .models
                .iter()
                .find(|model| model.id == choice.model_id)?
                .context_limit
        };
        let answering = open.transcript.messages().iter().rev().find_map(|message| {
            message
                .info
                .assistant
                .as_ref()
                .and_then(|facts| facts.model.as_ref())
        });
        let composer = match self.model_control() {
            ModelControl::Model {
                provider_id,
                model_id,
                ..
            } => Some(ModelChoice {
                provider_id,
                model_id,
                variant: None,
            }),
            _ => None,
        };
        let limit = session
            .model
            .as_ref()
            .and_then(limit_of)
            .or_else(|| answering.and_then(limit_of))
            .or_else(|| composer.as_ref().and_then(limit_of));
        context_usage::context_usage(open.transcript.messages(), limit)
    }

    /// The Context sources section: the workspace's skill and connected MCP
    /// counts (`WorkStatusContextSection.tsx`), hidden while both are zero.
    /// Linked threads and pinned knowledge are not read yet.
    fn render_context_sources(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let (skills, mcp) = self.context_counts()?;
        if skills == 0 && mcp == 0 {
            return None;
        }
        let strings = locale::strings(cx);
        let count = |count: usize, single: &str, plural: &str| {
            let key = if count == 1 { single } else { plural };
            strings.format(key, &[("count", &count.to_string())])
        };
        let mut summary: Vec<SharedString> = Vec::new();
        if skills > 0 {
            summary.push(count(
                skills,
                "chat.workStatus.breakdown.skillCountSingle",
                "chat.workStatus.breakdown.skillCountPlural",
            ));
        }
        if mcp > 0 {
            summary.push(count(
                mcp,
                "chat.workStatus.breakdown.mcpCountSingle",
                "chat.workStatus.breakdown.mcpCountPlural",
            ));
        }
        let rows = vec![
            work_status_row(
                strings.get("chat.workStatus.breakdown.skills"),
                skills.to_string().into(),
                None,
                palette,
                direction,
                cx,
            ),
            work_status_row(
                strings.get("chat.workStatus.breakdown.mcp"),
                mcp.to_string().into(),
                None,
                palette,
                direction,
                cx,
            ),
        ];
        Some(collapsible_section(
            Section {
                id: "work-status-context",
                icon: "stack",
                title: strings.get("chat.workStatus.section.contextBreakdown"),
                summary: Some(SharedString::from(summary.join(" · "))),
                expanded: self.work_status_context_open,
                toggle: |this| this.work_status_context_open = !this.work_status_context_open,
            },
            rows,
            palette,
            direction,
            cx,
        ))
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

/// A collapsible section of the card (`WorkStatusSection` with a toggle).
pub(crate) struct Section {
    pub id: &'static str,
    pub icon: &'static str,
    pub title: SharedString,
    /// Shown at the heading's end, in muted text.
    pub summary: Option<SharedString>,
    pub expanded: bool,
    pub toggle: fn(&mut Workspace),
}

/// The section's 24 px heading (its glyph, title, chevron and summary) and,
/// while expanded, its rows; `mt-5` above.
pub(crate) fn collapsible_section(
    section: Section,
    rows: Vec<AnyElement>,
    palette: &Palette,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    let foreground = palette.foreground;
    let toggle = section.toggle;
    let heading = on_activate(
        div()
            .id(section.id)
            .role(Role::Button)
            .accessibility_id(section.id)
            .aria_label(section.title.clone())
            .aria_expanded(section.expanded)
            .flex_1()
            .min_w_0()
            .h_full()
            .px(px(4.0))
            .rounded(px(CONTROL_RADIUS))
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .text_color(palette.muted_foreground)
            .hover(move |style| style.text_color(foreground))
            .child(glyph(section.icon, 16.0, palette.muted_foreground))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .css_text(XS_SIZE, XS_LINE, cx)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(palette.foreground)
                    .child(section.title),
            )
            .child(sidebar_view::chevron(!section.expanded, direction, palette))
            .child(div().flex_1())
            .children(section.summary.map(|summary| {
                div()
                    .min_w_0()
                    .max_w(gpui_kit::relative(0.6))
                    .truncate()
                    .css_text(XS_SIZE, XS_LINE, cx)
                    .text_color(palette.muted_foreground)
                    .child(summary)
            })),
        cx,
        move |this, _, cx| {
            toggle(this);
            cx.notify();
        },
    )
    .test_support();
    div()
        .mt(px(SECTION_GAP))
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(HEADING_HEIGHT))
                .mb(px(2.0))
                .flex_row_from(direction)
                .items_center()
                .child(heading),
        )
        .when(section.expanded, |this| this.children(rows))
        .into_any_element()
}

/// A 28 px readout (`WorkStatusRow`): a label that truncates before its
/// value, both muted unless the value has a tone.
pub(crate) fn work_status_row(
    label: SharedString,
    value: SharedString,
    value_color: Option<gpui_kit::Hsla>,
    palette: &Palette,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    div()
        .h(px(ROW_HEIGHT))
        .px(px(4.0))
        .flex_row_from(direction)
        .items_center()
        .gap(px(8.0))
        .text_color(palette.muted_foreground)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .css_text(ROW_SIZE, ROW_LINE, cx)
                .child(label),
        )
        .child(
            div()
                .flex_shrink_0()
                .css_text(ROW_SIZE, ROW_LINE, cx)
                .when_some(value_color, |this, color| this.text_color(color))
                .child(value),
        )
        .into_any_element()
}

/// One value at a row's end, in its own tone or the row's muted text.
pub(crate) struct RowValue {
    pub text: SharedString,
    pub color: Option<gpui_kit::Hsla>,
}

/// A readout with a leading glyph (`WorkStatusRow` with `icon`).
pub(crate) struct IconRow {
    pub icon: &'static str,
    pub label: SharedString,
    pub values: Vec<RowValue>,
}

/// A 28 px row: its 16 px glyph, a label that truncates before the values,
/// and the values `gap-1.5` apart, all muted unless toned.
pub(crate) fn icon_row(
    row: IconRow,
    palette: &Palette,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    let values = (!row.values.is_empty()).then(|| {
        div()
            .flex_shrink_0()
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .children(row.values.into_iter().map(|value| {
                div()
                    .css_text(ROW_SIZE, ROW_LINE, cx)
                    .when_some(value.color, |this, color| this.text_color(color))
                    .child(value.text)
            }))
    });
    div()
        .h(px(ROW_HEIGHT))
        .px(px(4.0))
        .flex_row_from(direction)
        .items_center()
        .gap(px(8.0))
        .text_color(palette.muted_foreground)
        .child(glyph(row.icon, 16.0, palette.muted_foreground))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .css_text(ROW_SIZE, ROW_LINE, cx)
                .child(row.label),
        )
        .children(values)
        .into_any_element()
}
