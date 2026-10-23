//! The prompt navigator: a column of ticks at the transcript's end edge, one
//! per prompt, the current one longer (`PromptNavigatorRail.tsx`).
//!
//! The current prompt follows the reading line as `scrollSpy.ts` places it:
//! the last prompt whose top is at or above the middle of the viewport, or the
//! last prompt once the reader is near the end. Hovering, previews and
//! selecting a prompt arrive with the rail's pointer behavior.

use gpui_kit::{
    AnyElement, InteractiveElement, IntoElement, ListState, ParentElement, Role,
    StatefulInteractiveElement, Styled, Window, div, px, relative,
};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::transcript::{PartContent, Role as MessageRole, Transcript};

use crate::chrome;
use crate::locale::Direction;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// `inset-e-3.5`: the ticks keep clear of the transcript's scrollbar.
const RIGHT_OFFSET: f32 = 14.0;
const GUTTER_WIDTH: f32 = 28.0;
/// The gutter once the message column reaches under the full one; it clips
/// the ticks to its width.
const NARROW_GUTTER_WIDTH: f32 = 12.0;
const MAX_VISIBLE_TICKS: usize = 30;
const TICK_PITCH: f32 = 12.0;
/// The ticks' `inset-e-1` inside the gutter and their `h-0.5`.
const TICK_INSET_END: f32 = 4.0;
const TICK_HEIGHT: f32 = 2.0;
const TICK_WIDTH: f32 = 10.0;
const ACTIVE_TICK_WIDTH: f32 = 14.0;
/// Inactive ticks draw the surface foreground at 30%.
const INACTIVE_ALPHA: f32 = 0.3;
/// The share of the gutter that fades out where more ticks lie beyond.
const FADE_SHARE: f32 = 0.14;
/// `scrollSpy.ts`: the reading line, and the bottom zone where the last
/// prompt is current.
const READ_LINE_FACTOR: f32 = 0.5;
const BOTTOM_ANCHOR_MIN: f32 = 48.0;
const BOTTOM_ANCHOR_FACTOR: f32 = 0.1;

/// The transcript rows of the prompts the rail lists: user messages with
/// something to show.
fn prompt_rows(transcript: &Transcript) -> Vec<usize> {
    transcript
        .messages()
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.info.role == MessageRole::User
                && message.parts.iter().any(|part| match &part.content {
                    PartContent::Text { text } => !text.trim().is_empty(),
                    PartContent::File { .. } => true,
                    _ => false,
                })
        })
        .map(|(row, _)| row)
        .collect()
}

/// The index into `prompts` of the current prompt, from the list's last
/// layout; `None` before the list was laid out.
fn current_prompt(list: &ListState, prompts: &[usize]) -> Option<usize> {
    let viewport = list.viewport_bounds();
    let height = f32::from(viewport.size.height);
    if prompts.is_empty() || height <= 0.0 {
        return None;
    }
    let scrolled = -f32::from(list.scroll_px_offset_for_scrollbar().y);
    let from_bottom = f32::from(list.max_offset_for_scrollbar().y) - scrolled;
    if from_bottom <= BOTTOM_ANCHOR_MIN.max(height * BOTTOM_ANCHOR_FACTOR) {
        return Some(prompts.len() - 1);
    }
    let line = f32::from(viewport.top()) + height * READ_LINE_FACTOR;
    let first_shown = list.logical_scroll_top().item_ix;
    let mut current = 0;
    for (index, &row) in prompts.iter().enumerate() {
        let above = row < first_shown
            || list
                .bounds_for_item(row)
                .is_some_and(|bounds| f32::from(bounds.top()) <= line);
        if !above {
            break;
        }
        current = index;
    }
    Some(current)
}

impl Workspace {
    /// Records the selected session's current prompt; whether it changed, so
    /// a scroll that moves it redraws the rail.
    pub(crate) fn refresh_prompt_rail(&mut self, session: &SessionId) -> bool {
        if self.selected.as_ref() != Some(session) {
            return false;
        }
        let Some(open) = self.open_sessions.get_mut(session) else {
            return false;
        };
        let current = current_prompt(&open.list, &prompt_rows(&open.transcript));
        let changed = open.rail_current != current;
        open.rail_current = current;
        changed
    }

    /// The rail of the selected session, when it has at least two prompts.
    pub(crate) fn render_prompt_rail(
        &self,
        palette: &Palette,
        direction: Direction,
        label: gpui_kit::SharedString,
        window: &Window,
    ) -> Option<AnyElement> {
        let open = self.open_sessions.get(self.selected.as_ref()?)?;
        let prompts = prompt_rows(&open.transcript);
        if prompts.len() < 2 {
            return None;
        }
        let total = prompts.len();
        let current = current_prompt(&open.list, &prompts).unwrap_or(total - 1);
        let count = total.min(MAX_VISIBLE_TICKS);
        // The window glides so the current prompt stays in its middle.
        let start = current.saturating_sub(count / 2).min(total - count);
        let more_above = start > 0;
        let more_below = start + count < total;
        let height = count as f32 * TICK_PITCH;
        // The message column, centered in the area left of the navigation
        // gutter, may reach under the full gutter (`isNarrowGutter`).
        let area = self.chat_area_width(window);
        let inner = area - chrome::NAVIGATION_GUTTER;
        let column_end = (inner + inner.min(chrome::CHAT_COLUMN_MAX_WIDTH)) / 2.0;
        let gutter = if column_end > area - RIGHT_OFFSET - GUTTER_WIDTH {
            NARROW_GUTTER_WIDTH
        } else {
            GUTTER_WIDTH
        };
        let ticks = (0..count).map(|slot| {
            let index = start + slot;
            let active = index == current;
            let middle = (slot as f32 + 0.5) * TICK_PITCH / height;
            let mut fade: f32 = 1.0;
            if more_above && middle < FADE_SHARE {
                fade = fade.min(middle / FADE_SHARE);
            }
            if more_below && middle > 1.0 - FADE_SHARE {
                fade = fade.min((1.0 - middle) / FADE_SHARE);
            }
            let alpha = if active { 1.0 } else { INACTIVE_ALPHA };
            div()
                .absolute()
                .inset_end(direction, px(TICK_INSET_END))
                .top(px(slot as f32 * TICK_PITCH))
                .h(px(TICK_PITCH))
                .flex()
                .items_center()
                .child(
                    div()
                        .h(px(TICK_HEIGHT))
                        .w(px(if active {
                            ACTIVE_TICK_WIDTH
                        } else {
                            TICK_WIDTH
                        }))
                        .rounded_full()
                        .bg(palette.foreground.opacity(alpha * fade)),
                )
        });
        Some(
            div()
                .id("prompt-rail")
                .role(Role::Navigation)
                .accessibility_id("prompt-rail")
                .aria_label(label)
                .absolute()
                .inset_end(direction, px(RIGHT_OFFSET))
                .top(relative(0.5))
                .mt(px(-height / 2.0))
                .w(px(gutter))
                .h(px(height))
                .overflow_hidden()
                .children(ticks)
                .into_any_element(),
        )
    }
}
