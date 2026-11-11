//! The header controls that stay put through a session
//! (`HeaderContextUsage.tsx`, `OpenInAppButton.tsx`): the context window's
//! fill as a ring with its percentage, and the button that opens the shown
//! directory in the file manager. They sit before the project action.
//!
//! The ring's track is a 2 px border and its arc a 2 px stroke on the same
//! band: the reference's 16/9 px stroke lands on two device pixels at the
//! default scale. The Open In button's menu (copy path, other apps) is not
//! ported; the file manager is the only app this client opens.

use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::path::PathBuf;

use gpui_kit::{
    AnyElement, Bounds, Context, Corners, FontWeight, Hsla, InteractiveElement, IntoElement,
    ParentElement, PathBuilder, Pixels, Role, SharedString, StatefulInteractiveElement, Styled,
    TestSupportExt, canvas, div, fill, point, px, size,
};
use openchamber_native_core::context_usage::{self, ContextUsage, UsageTone};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, IconButton};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::open_in_app;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The control names, which automation and tests find them by.
pub(crate) const CONTEXT_USAGE_ID: &str = "header-context-usage";
pub(crate) const OPEN_IN_ID: &str = "header-open-in";

/// `size-4`.
const RING: f32 = 16.0;
const RING_STROKE: f32 = 2.0;
/// `typography-meta`: 12.5 px on an 18 px line.
const META_SIZE: f32 = 0.781_25;
const META_LINE: f32 = 1.125;
/// The reading while a finished compaction is newer than any response.
const UNKNOWN_PERCENT: &str = "\u{2014}";

impl Workspace {
    /// The selected session's context fill, absent before any response
    /// reported tokens. A click returns to the session context, as the
    /// rail's Context button does.
    pub(crate) fn render_header_context_usage(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let usage = self.session_context_usage()?;
        let (text, percent) = match usage {
            ContextUsage::Measured { percent, .. } => {
                (context_usage::format_percent(percent), Some(percent))
            }
            ContextUsage::Compacted { .. } => (UNKNOWN_PERCENT.to_owned(), None),
        };
        // Rounded for the tone, as the work status meter does, so both turn
        // together.
        let arc = percent.map(|percent| {
            let color = match UsageTone::of(percent) {
                UsageTone::Safe => palette.status_success,
                UsageTone::Warn => palette.status_warning,
                UsageTone::Critical => palette.status_error,
            };
            ((percent / 100.0).clamp(0.0, 1.0) as f32, color)
        });
        // This client has no context overview surface to mark as shown, so
        // the button always rests muted.
        let rest = palette.muted_foreground;
        let foreground = palette.foreground;
        let mut ring = div().relative().size(px(RING)).flex_shrink_0().child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size(px(RING))
                .rounded_full()
                .border_2()
                .border_color(rest.opacity(0.3))
                .group_hover(CONTEXT_USAGE_ID, move |style| {
                    style.border_color(foreground.opacity(0.3))
                }),
        );
        if let Some((fraction, color)) = arc.filter(|(fraction, _)| *fraction > 0.0) {
            ring = ring.child(
                canvas(
                    |_, _, _| {},
                    move |bounds, (), window, _| paint_arc(bounds, fraction, color, window),
                )
                .absolute()
                .top_0()
                .left_0()
                .size(px(RING)),
            );
        }
        let button = div()
            .id(CONTEXT_USAGE_ID)
            .group(CONTEXT_USAGE_ID)
            .role(Role::Button)
            .accessibility_id(CONTEXT_USAGE_ID)
            .aria_label(locale::strings(cx).get("contextUsage.aria.label"))
            .aria_description(SharedString::from(text.clone()))
            .h(px(24.0))
            .px(px(4.0))
            .flex_shrink_0()
            .rounded(px(CONTROL_RADIUS))
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .text_color(rest)
            .hover(move |style| style.text_color(foreground))
            // A press here is the button's, never the start of a window move.
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .child(ring)
            .child(
                div()
                    .css_text(META_SIZE, META_LINE, cx)
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(text)),
            );
        Some(
            on_activate(button, cx, |this, _, cx| {
                if this.panels.open.take().is_some() {
                    cx.notify();
                }
            })
            .test_support()
            .into_any_element(),
        )
    }

    /// The Open In button: the file manager on the directory the header's
    /// project controls report on, for a server on this machine.
    pub(crate) fn render_header_open_in(
        &self,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.desktop_local() {
            return None;
        }
        let subject = self.project_subject()?;
        let label = locale::strings(cx).format(
            "openInApp.actions.openInAria",
            &[("app", open_in_app::LABEL)],
        );
        let directory = PathBuf::from(subject.directory);
        Some(
            IconButton::new(OPEN_IN_ID, "folder", label).render(palette, cx, move |_, _, _| {
                if let Err(error) = open_in_app::open_directory(&directory) {
                    tracing::warn!(%error, "the file manager could not be opened");
                }
            }),
        )
    }
}

/// Where the arc's centre line is at `fraction` of a turn from twelve
/// o'clock, clockwise.
fn arc_point(bounds: Bounds<Pixels>, fraction: f32) -> gpui_kit::Point<Pixels> {
    let radius = (RING - RING_STROKE) / 2.0;
    let angle = fraction * TAU - FRAC_PI_2;
    point(
        bounds.origin.x + px(RING / 2.0 + radius * angle.cos()),
        bounds.origin.y + px(RING / 2.0 + radius * angle.sin()),
    )
}

/// The filled share of the ring: a stroke from twelve o'clock, clockwise,
/// with round ends.
fn paint_arc(bounds: Bounds<Pixels>, fraction: f32, color: Hsla, window: &mut gpui_kit::Window) {
    let radius = (RING - RING_STROKE) / 2.0;
    let radii = point(px(radius), px(radius));
    let mut path = PathBuilder::stroke(px(RING_STROKE));
    path.move_to(arc_point(bounds, 0.0));
    if fraction >= 1.0 {
        // One arc cannot return to its start: a full ring is two halves.
        path.arc_to(radii, px(0.0), false, true, arc_point(bounds, 0.5));
        path.arc_to(radii, px(0.0), false, true, arc_point(bounds, 1.0));
    } else {
        path.arc_to(
            radii,
            px(0.0),
            fraction * TAU > PI,
            true,
            arc_point(bounds, fraction),
        );
    }
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
    for end in [0.0, fraction.min(1.0)] {
        let centre = arc_point(bounds, end);
        let cap = Bounds::new(
            point(
                centre.x - px(RING_STROKE / 2.0),
                centre.y - px(RING_STROKE / 2.0),
            ),
            size(px(RING_STROKE), px(RING_STROKE)),
        );
        window.paint_quad(fill(cap, color).corner_radii(Corners::all(px(RING_STROKE / 2.0))));
    }
}

#[cfg(test)]
mod tests {
    use super::{RING, arc_point};
    use gpui_kit::{Bounds, point, px, size};

    #[test]
    fn the_arc_starts_at_twelve_and_runs_clockwise() {
        let bounds = Bounds::new(point(px(10.0), px(20.0)), size(px(RING), px(RING)));
        let near = |actual: gpui_kit::Point<gpui_kit::Pixels>, x: f32, y: f32| {
            (f32::from(actual.x) - x).abs() < 0.001 && (f32::from(actual.y) - y).abs() < 0.001
        };
        assert!(near(arc_point(bounds, 0.0), 18.0, 21.0), "twelve o'clock");
        assert!(near(arc_point(bounds, 0.25), 25.0, 28.0), "three o'clock");
        assert!(near(arc_point(bounds, 0.5), 18.0, 35.0), "six o'clock");
        assert!(near(arc_point(bounds, 0.75), 11.0, 28.0), "nine o'clock");
    }
}
