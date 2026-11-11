//! The work status panel beside the chat and the panel rail at the end edge.
//!
//! Mirrors `packages/ui/src/components/layout/ContextPanelRail.tsx`, the rail
//! surfaces of `packages/ui/src/lib/surfaces/registry.ts` and the docked work
//! status card (`.oc-panel-edge`).

mod mcp_section;
pub(crate) mod pinned_section;
#[cfg(test)]
mod pinned_section_tests;
pub(crate) mod subagents_section;
pub(crate) mod work_status_sections;
mod work_status_usage;
mod workflows_section;

use gpui_kit::{
    AnyElement, App, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, div, prelude::FluentBuilder,
    px,
};

use openchamber_native_core::context_usage::{self, ContextUsage, UsageTone};
use openchamber_native_core::send_config::ModelChoice;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, IconButton, glyph};
use crate::elevation;
use crate::files_view::PanelSurface;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::send_catalog::ModelControl;
use crate::sidebar_view;
use crate::theme::Palette;
use crate::ui_memory::{self, UiMemory};
use crate::workspace::Workspace;

/// Width of the docked work status panel.
pub const PANEL_WIDTH: f32 = 324.0;
/// `WORK_STATUS_REQUIRED_ROW_WIDTH`: the panel column plus 560 px of chat.
const REQUIRED_CHAT_AREA_WIDTH: f32 = PANEL_WIDTH + 560.0;
/// Width of the rail.
pub const RAIL_WIDTH: f32 = 44.0;
const CARD_RADIUS: f32 = 12.0;
const RAIL_BUTTON: f32 = 28.0;
const RAIL_GLYPH: f32 = 17.0;
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
    ("rail-browser", "global", "contextPanel.mode.browser"),
    ("rail-notes", "book-marked", "contextRail.surface.notes"),
];
/// The surfaces that open a group (`BUILT_IN_SURFACE_GROUPS`): the session's
/// changes, the tools that work on the project and the session's workspace.
/// A divider precedes each, the first one after the session's context.
const RAIL_GROUP_STARTS: [&str; 3] = ["rail-git", "rail-files", "rail-notes"];

impl Workspace {
    pub(crate) fn work_status_fits(&self, window: &gpui_kit::Window) -> bool {
        let sidebar = if self.sidebar_open {
            self.pane_sizes.sidebar_width()
        } else {
            0.0
        };
        f32::from(crate::chrome::content_size(window).width) - sidebar - RAIL_WIDTH
            >= REQUIRED_CHAT_AREA_WIDTH
    }

    pub(crate) fn work_status_inline(&self, window: &gpui_kit::Window) -> bool {
        self.work_status_open && self.work_status_fits(window)
    }

    /// The docked work status card.
    pub(crate) fn render_work_status_panel(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let strings = locale::strings(cx);
        let layout = work_status_sections::Layout::for_workspace(self, cx);
        let foreground = palette.foreground;
        let mut sections = Vec::new();
        for id in layout.visible_builtin_order() {
            let first_heading = sections.is_empty();
            let section = match id {
                "session" => {
                    Some(self.render_work_status_session(palette, direction, first_heading, cx))
                }
                "repository" => self.render_project_section(palette, direction, first_heading, cx),
                "telemetry" => self.render_turn_stats(palette, direction, first_heading, cx),
                "contextSources" => {
                    self.render_context_sources(palette, direction, first_heading, cx)
                }
                "usage" => self.render_work_status_usage(palette, direction, first_heading, cx),
                "subagents" => self.render_subagents(palette, direction, first_heading, cx),
                "workflows" => self.render_workflows_section(palette, direction, first_heading, cx),
                "mcp" => self.render_mcp_section(palette, direction, first_heading, cx),
                "pinned" => self.render_pinned_messages(palette, direction, first_heading, cx),
                _ => unreachable!("visible_builtin_order yields only canonical built-ins"),
            };
            if let Some(section) = section {
                sections.push(section);
            }
        }
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
                            .flex()
                            .flex_col()
                            .gap(px(SECTION_GAP))
                            .children(sections),
                    )
                    .when(layout.all_hidden(), |card| {
                        card.child(
                            div()
                                .id("work-status-all-hidden")
                                .accessibility_id("work-status-all-hidden")
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .text_center()
                                .px(px(16.0))
                                .py(px(32.0))
                                .css_text(0.875, 1.25, cx)
                                .text_color(palette.muted_foreground)
                                .child(strings.get("chat.workStatus.sections.allHidden"))
                                .child(
                                    on_activate(
                                        div()
                                            .id("work-status-sections-reopen")
                                            .accessibility_id("work-status-sections-reopen")
                                            .role(Role::Button)
                                            .aria_label(
                                                strings.get("chat.workStatus.sections.open"),
                                            )
                                            .tab_index(0)
                                            .mt(px(8.0))
                                            .css_text(0.75, 1.0, cx)
                                            .cursor_pointer()
                                            .hover(move |style| style.text_color(foreground))
                                            .child(strings.get("chat.workStatus.sections.open")),
                                        cx,
                                        |this, window, cx| {
                                            this.open_work_status_sections(window, cx)
                                        },
                                    )
                                    .test_support(),
                                ),
                        )
                    })
                    .child(
                        div()
                            .absolute()
                            .top(px(6.0))
                            .inset_end(direction, px(8.0))
                            .child(
                                on_activate(
                                    div()
                                        .id("work-status-sections")
                                        .group("work-status-sections-trigger")
                                        .accessibility_id("work-status-sections")
                                        .role(Role::Button)
                                        .aria_label(strings.get("chat.workStatus.sections.open"))
                                        .tab_index(0)
                                        .size(px(20.0))
                                        .rounded(px(CONTROL_RADIUS))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .text_color(palette.muted_foreground)
                                        .hover(move |style| style.text_color(foreground))
                                        .child(
                                            glyph("equalizer-2", 16.0, palette.muted_foreground)
                                                .group_hover(
                                                    "work-status-sections-trigger",
                                                    move |style| style.text_color(foreground),
                                                ),
                                        ),
                                    cx,
                                    |this, window, cx| this.open_work_status_sections(window, cx),
                                )
                                .test_support(),
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

    fn render_work_status_session(
        &self,
        palette: &Palette,
        direction: Direction,
        first_heading: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_row_from(direction)
                    .items_center()
                    .mb(px(2.0))
                    .px(px(4.0))
                    .when(first_heading, |heading| {
                        heading.padding_end(direction, px(28.0))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .css_text(XS_SIZE, XS_LINE, cx)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(palette.foreground)
                            .child(locale::strings(cx).get("chat.workStatus.section.session")),
                    ),
            )
            .children(self.render_context_usage(palette, direction, cx))
            .into_any_element()
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
            .child(glyph("donut-chart", 14.0, palette.muted_foreground))
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
    pub(crate) fn session_context_usage(&self) -> Option<ContextUsage> {
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
        first_heading: bool,
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
                toggle: |this, _| this.work_status_context_open = !this.work_status_context_open,
            },
            rows,
            palette,
            direction,
            first_heading,
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
                .child(
                    glyph("donut-chart-fill", RAIL_GLYPH, palette.muted_foreground)
                        .group_hover("rail-context", move |style| style.text_color(foreground)),
                ),
            cx,
            // The session context replaces an open server surface.
            |this, _, cx| {
                if this.panels.open.take().is_some() {
                    cx.notify();
                }
            },
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
            .children(RAIL_SURFACES.iter().flat_map(|(id, icon, key)| {
                let divider = RAIL_GROUP_STARTS
                    .contains(id)
                    .then(|| rail_divider(palette));
                let surface = match *id {
                    "rail-files" => Some(PanelSurface::Files),
                    "rail-git" => Some(PanelSurface::Git),
                    "rail-diff" => Some(PanelSurface::Diff),
                    "rail-browser" => Some(PanelSurface::Browser),
                    "rail-notes" => Some(PanelSurface::Notes),
                    _ => None,
                };
                let open = (surface.is_some() && self.panels.open == surface)
                    || (*id == "rail-terminal" && self.terminal_open);
                let button = IconButton::new(*id, icon, strings.get(key))
                    .size(RAIL_BUTTON, RAIL_GLYPH)
                    .tone(if open {
                        crate::chrome::ButtonTone::Active
                    } else {
                        crate::chrome::ButtonTone::Quiet
                    })
                    .render(palette, cx, move |this, window, cx| {
                        if let Some(surface) = surface {
                            this.toggle_panel(surface, cx);
                        } else if *id == "rail-terminal" {
                            this.toggle_terminal(window, cx);
                        }
                    });
                if *id != "rail-diff" {
                    return divider.into_iter().chain([button]);
                }
                // The changes surface tints its start column removed and its
                // end column added.
                let (start, end) = match direction {
                    Direction::LeftToRight => (palette.status_error, palette.status_success),
                    Direction::RightToLeft => (palette.status_success, palette.status_error),
                };
                let button = div()
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
                    .into_any_element();
                divider.into_iter().chain([button])
            }))
            // Installed extensions are a group of their own.
            .children({
                let guests = self.render_guest_rail(palette, cx);
                (!guests.is_empty())
                    .then(|| rail_divider(palette))
                    .into_iter()
                    .chain(guests)
            })
            .child(
                IconButton::new(
                    "rail-configure",
                    "equalizer-2",
                    strings.get("contextRail.configure.open"),
                )
                .size(RAIL_BUTTON, RAIL_GLYPH)
                .render(palette, cx, |_, _, _| {}),
            )
    }
}

/// The line between two groups of the rail (`h-px w-4 bg-edge-strong`).
fn rail_divider(palette: &Palette) -> AnyElement {
    div()
        .h(px(1.0))
        .w(px(16.0))
        .flex_shrink_0()
        .bg(palette.border)
        .into_any_element()
}

impl Workspace {
    /// A rail button per extension panel of the selected runtime.
    fn render_guest_rail(&self, palette: &Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some((runtime, panels)) = &self.panels.guest_catalog else {
            return Vec::new();
        };
        if *runtime != self.runtime {
            return Vec::new();
        }
        panels
            .iter()
            .map(|panel| {
                // A Remixicon the export lacks, or a package SVG, falls back
                // to `apps`, as the rail's sprite does.
                let icon = crate::assets::icon_name(&panel.icon).unwrap_or("apps");
                let open = self.panels.open == Some(PanelSurface::Guest)
                    && self
                        .panels
                        .open_guest
                        .as_ref()
                        .is_some_and(|open| open.id == panel.id);
                let toggled = panel.clone();
                IconButton::new(
                    SharedString::from(format!("rail-guest-{}", panel.id)),
                    icon,
                    SharedString::from(panel.name.clone()),
                )
                .size(RAIL_BUTTON, RAIL_GLYPH)
                .tone(if open {
                    crate::chrome::ButtonTone::Active
                } else {
                    crate::chrome::ButtonTone::Quiet
                })
                .render(palette, cx, move |this, _, cx| {
                    this.toggle_guest_panel(toggled.clone(), cx)
                })
            })
            .collect()
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
    /// Flips the section's stored state; the heading redraws afterwards.
    pub toggle: fn(&mut Workspace, &mut Context<Workspace>),
}

/// Whether the work status section `id` shows its rows: the user's stored
/// choice (`workStatusExpandedSections`), or `default` before they made one.
pub(crate) fn section_expanded(id: &str, default: bool, cx: &App) -> bool {
    cx.try_global::<UiMemory>()
        .and_then(|memory| memory.work_status_section_expanded(id))
        .unwrap_or(default)
}

/// Stores the opposite of what [`section_expanded`] reports for `id`.
pub(crate) fn toggle_section(id: &str, default: bool, cx: &mut App) {
    let expanded = section_expanded(id, default, cx);
    ui_memory::set_work_status_section_expanded(id, !expanded, cx);
}

/// The section's 24 px heading (its glyph, title, chevron and summary) and,
/// while expanded, its rows. The containing list owns section spacing.
pub(crate) fn collapsible_section(
    section: Section,
    rows: Vec<AnyElement>,
    palette: &Palette,
    direction: Direction,
    first_heading: bool,
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
            .child(glyph(section.icon, 14.0, palette.muted_foreground))
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
            toggle(this, cx);
            cx.notify();
        },
    )
    .test_support();
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(HEADING_HEIGHT))
                .mb(px(2.0))
                .flex_row_from(direction)
                .items_center()
                .when(first_heading, |heading| {
                    heading.padding_end(direction, px(28.0))
                })
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
        .child(glyph(row.icon, 14.0, palette.muted_foreground))
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

/// How a row's trailing pill reads (`WorkStatusRowAction`'s `tone`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActionTone {
    Default,
    Info,
    Warning,
    Error,
}

/// A row's trailing control shaped like a badge (`WorkStatusRowAction`): a
/// status that is also the thing to press. A press never reaches the row
/// under it. While `disabled` the pill is dimmed and a press on it does
/// nothing, the row included.
pub(crate) fn row_action(
    id: impl Into<SharedString>,
    label: SharedString,
    tone: ActionTone,
    disabled: bool,
    palette: &Palette,
    cx: &Context<Workspace>,
    action: impl Fn(&mut Workspace, &mut gpui_kit::Window, &mut Context<Workspace>) + 'static,
) -> AnyElement {
    let id: SharedString = id.into();
    let color = match tone {
        ActionTone::Default => None,
        ActionTone::Info => Some(palette.status_info),
        ActionTone::Warning => Some(palette.status_warning),
        ActionTone::Error => Some(palette.status_error),
    };
    let pill = div()
        .id(id.clone())
        .role(Role::Button)
        .accessibility_id(id)
        .aria_label(label.clone())
        .flex_shrink_0()
        .px(px(6.0))
        .py(px(1.0))
        .rounded_full()
        .font_weight(FontWeight::MEDIUM)
        .text_color(color.unwrap_or(palette.muted_foreground))
        // `color-mix(in srgb, <tone> 18%, transparent)`.
        .bg(color.map_or(palette.hover, |color| color.opacity(0.18)))
        .child(div().css_text(CAPTION_SIZE, CAPTION_LINE, cx).child(label));
    if disabled {
        return pill
            .opacity(0.5)
            // The press stops here, as a disabled button's does in the web.
            .on_click(|_, _, cx| cx.stop_propagation())
            .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
            .test_support()
            .into_any_element();
    }
    on_activate(
        pill.cursor_pointer().hover(|style| style.opacity(0.8)),
        cx,
        move |this, window, cx| {
            cx.stop_propagation();
            action(this, window, cx);
        },
    )
    .test_support()
    .into_any_element()
}
