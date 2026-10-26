//! Prompt navigation through a corner indicator and a scrollable prompt list.
//!
//! The current prompt follows the reading line as `scrollSpy.ts` places it:
//! the last prompt whose top is at or above the middle of the viewport, or the
//! last prompt once the reader is near the end. The panel opens on hover or
//! click, or through the keyboard shortcut, and retains its own scroll and
//! highlighted message while the transcript changes.

use gpui_kit::{
    AnyElement, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, ListState,
    ParentElement, Role, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    Subscription, TestSupportExt, Window, div, point, prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::transcript::{PartContent, Role as MessageRole, Transcript};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{self, glyph};
use crate::elevation;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::{EarlierHistory, Workspace};

const CORNER_INSET: f32 = 12.0;
const INDICATOR_TICKS: usize = 5;
const TICK_HEIGHT: f32 = 2.0;
const TICK_WIDTH: f32 = 12.0;
const PANEL_WIDTH: f32 = 320.0;
const PANEL_HEIGHT: f32 = 384.0;
const ROW_HEIGHT: f32 = 30.0;
/// Inactive ticks draw the surface foreground at 30%.
const INACTIVE_ALPHA: f32 = 0.3;
/// `scrollSpy.ts`: the reading line, and the bottom zone where the last
/// prompt is current.
const READ_LINE_FACTOR: f32 = 0.5;
const BOTTOM_ANCHOR_MIN: f32 = 48.0;
const BOTTOM_ANCHOR_FACTOR: f32 = 0.1;

pub(crate) struct PromptPanel {
    highlighted: MessageId,
    keyboard: bool,
    focus: FocusHandle,
    scroll: ScrollHandle,
    _blur: Subscription,
}

struct PromptEntry {
    id: MessageId,
    row: usize,
    preview: SharedString,
}

fn prompt_entries(transcript: &Transcript, empty: &SharedString) -> Vec<PromptEntry> {
    prompt_rows(transcript)
        .into_iter()
        .map(|row| {
            let message = &transcript.messages()[row];
            let preview = message
                .plain_text()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            PromptEntry {
                id: message.info.id.clone(),
                row,
                preview: if preview.is_empty() {
                    empty.clone()
                } else {
                    preview.chars().take(160).collect::<String>().into()
                },
            }
        })
        .collect()
}

fn indicator_tick(current: usize, total: usize) -> usize {
    if total <= 1 {
        return 0;
    }
    ((current as f32 / (total - 1) as f32) * (total.min(INDICATOR_TICKS) - 1) as f32).round()
        as usize
}

/// The transcript rows of the prompts the rail lists: user messages with
/// something to show.
pub(crate) fn prompt_rows(transcript: &Transcript) -> Vec<usize> {
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
    pub(crate) fn toggle_prompt_navigator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .prompt_panel
            .as_ref()
            .is_some_and(|panel| panel.keyboard)
        {
            self.close_prompt_panel(window, cx);
        } else {
            self.open_prompt_panel(true, window, cx);
        }
    }

    pub(crate) fn prompt_history_prepended(&mut self, count: usize) {
        if let Some(panel) = &self.prompt_panel {
            let mut offset = panel.scroll.offset();
            offset.y -= px(count as f32 * ROW_HEIGHT);
            panel.scroll.set_offset(offset);
        }
    }
    pub(crate) fn open_prompt_panel(
        &mut self,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.prompt_navigator_enabled)
            == Some(false)
        {
            return;
        }
        self.prompt_hover_task = None;
        if let Some(panel) = self.prompt_panel.as_mut() {
            if keyboard {
                panel.keyboard = true;
                window.focus(&panel.focus, cx);
            }
            return;
        }
        let Some(open) = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
        else {
            return;
        };
        let prompts = prompt_rows(&open.transcript);
        if prompts.len() < 2 {
            return;
        }
        let current = current_prompt(&open.list, &prompts).unwrap_or(prompts.len() - 1);
        let highlighted = open.transcript.messages()[prompts[current]].info.id.clone();
        let scroll = ScrollHandle::default();
        let height = PANEL_HEIGHT
            .min(f32::from(chrome::content_size(window).height) - chrome::TITLE_ROW_HEIGHT - 24.0)
            - 8.0;
        scroll.set_offset(point(
            px(0.0),
            px(-(current as f32 * ROW_HEIGHT - (height - ROW_HEIGHT) / 2.0).max(0.0)),
        ));
        let focus = cx.focus_handle();
        let blur = cx.on_blur(&focus, window, |this, _, cx| {
            if this
                .prompt_panel
                .as_ref()
                .is_some_and(|panel| panel.keyboard)
            {
                this.prompt_panel = None;
                cx.notify();
            }
        });
        if keyboard {
            window.focus(&focus, cx);
        }
        self.prompt_panel = Some(PromptPanel {
            highlighted,
            keyboard,
            focus,
            scroll,
            _blur: blur,
        });
        cx.notify();
    }

    pub(crate) fn close_prompt_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_hover_task = None;
        let focus_composer = self
            .prompt_panel
            .take()
            .is_some_and(|panel| panel.keyboard && panel.focus.is_focused(window));
        if focus_composer {
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    fn hover_prompt_panel(&mut self, hovered: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_hover_task = None;
        if hovered && (self.prompt_hover_suppressed || self.prompt_panel.is_some()) {
            return;
        }
        if !hovered {
            self.prompt_hover_suppressed = false;
            if self
                .prompt_panel
                .as_ref()
                .is_none_or(|panel| panel.keyboard)
            {
                return;
            }
        }
        let delay = std::time::Duration::from_millis(if hovered { 120 } else { 160 });
        self.prompt_hover_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if hovered {
                    this.open_prompt_panel(false, window, cx);
                } else {
                    this.close_prompt_panel(window, cx);
                }
            });
        }));
    }

    fn prompt_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
        else {
            return;
        };
        let prompts = prompt_rows(&open.transcript);
        let Some(panel) = self.prompt_panel.as_mut() else {
            return;
        };
        if prompts.is_empty() {
            return;
        }
        let current = prompts
            .iter()
            .position(|&row| open.transcript.messages()[row].info.id == panel.highlighted)
            .unwrap_or(0);
        let key = event.keystroke.key.as_str();
        let control = event.keystroke.modifiers.control;
        let next = match (key, control) {
            ("up", _) | ("p", true) => current.saturating_sub(1),
            ("down", _) | ("n", true) => (current + 1).min(prompts.len() - 1),
            ("home", _) => 0,
            ("end", _) => prompts.len() - 1,
            ("enter" | "space", _) => {
                cx.stop_propagation();
                self.select_prompt(prompts[current], window, cx);
                return;
            }
            ("escape", _) => {
                cx.stop_propagation();
                self.close_prompt_panel(window, cx);
                return;
            }
            _ => return,
        };
        cx.stop_propagation();
        panel.highlighted = open.transcript.messages()[prompts[next]].info.id.clone();
        panel.scroll.scroll_to_item(next);
        cx.notify();
    }

    fn select_prompt(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
        {
            open.list.set_follow_mode(gpui_kit::FollowMode::Normal);
            open.list.scroll_to(gpui_kit::ListOffset {
                item_ix: row,
                offset_in_item: px(0.0),
            });
        }
        self.prompt_hover_suppressed = true;
        self.close_prompt_panel(window, cx);
    }

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

    /// The corner indicator or its open prompt list.
    pub(crate) fn render_prompt_rail(
        &self,
        palette: &Palette,
        direction: Direction,
        label: SharedString,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if self
            .send_catalog
            .settings
            .as_ref()
            .and_then(|settings| settings.prompt_navigator_enabled)
            == Some(false)
        {
            return None;
        }
        let open = self.open_sessions.get(self.selected.as_ref()?)?;
        let prompts = prompt_rows(&open.transcript);
        if prompts.len() < 2 {
            return None;
        }
        let total = prompts.len();
        let current = current_prompt(&open.list, &prompts).unwrap_or(total - 1);
        let content = if let Some(panel) = &self.prompt_panel {
            let empty = locale::strings(cx).get("chat.timeline.noTextContent");
            let entries = prompt_entries(&open.transcript, &empty);
            let loading = matches!(open.earlier, EarlierHistory::Loading { .. });
            let failure = match open.earlier {
                EarlierHistory::Failed(error) => Some(error),
                _ => None,
            };
            let load_more = open.older.as_ref().map(|_| {
                let label = locale::strings(cx).get("chat.promptNavigator.loadMore");
                on_activate(
                    div()
                        .id("prompt-load-earlier")
                        .role(Role::Button)
                        .accessibility_id("prompt-load-earlier")
                        .aria_label(label.clone())
                        .h(px(ROW_HEIGHT))
                        .flex_shrink_0()
                        .mx(px(4.0))
                        .mt(px(4.0))
                        .px(px(8.0))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(6.0))
                        .text_color(if failure.is_some() {
                            palette.error
                        } else {
                            palette.muted_foreground
                        })
                        .when_some(failure, |row, _| {
                            row.aria_description(
                                locale::strings(cx).get("chat.container.sessionLoadError.title"),
                            )
                        })
                        .child(glyph(
                            if loading { "loader-4" } else { "arrow-up-s" },
                            14.0,
                            palette.muted_foreground,
                        ))
                        .child(div().truncate().css_text(0.78125, 1.125, cx).child(label)),
                    cx,
                    |this, _, cx| this.load_earlier_prompts(cx),
                )
                .test_support()
            });
            let height = (entries.len() as f32 * ROW_HEIGHT
                + 8.0
                + if load_more.is_some() {
                    ROW_HEIGHT + 4.0
                } else {
                    0.0
                })
            .min(PANEL_HEIGHT)
            .min(f32::from(chrome::content_size(window).height) - chrome::TITLE_ROW_HEIGHT - 24.0);
            let list = div()
                .id("prompt-list")
                .role(Role::ListBox)
                .accessibility_id("prompt-list")
                .aria_label(label.clone())
                .track_focus(&panel.focus)
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&panel.scroll)
                .p(px(4.0))
                .on_key_down(
                    cx.listener(|this, event, window, cx| this.prompt_key(event, window, cx)),
                )
                .children(entries.into_iter().enumerate().map(|(index, entry)| {
                    let id = entry.id.clone();
                    let selected = panel.highlighted == entry.id;
                    let active = index == current;
                    on_activate(
                        div()
                            .id(SharedString::from(format!("prompt-row-{}", entry.id)))
                            .role(Role::ListBoxOption)
                            .accessibility_id(format!("prompt-row-{}", entry.id))
                            .aria_label(entry.preview.clone())
                            .aria_selected(selected)
                            .when(active, |row| {
                                row.aria_description(
                                    locale::strings(cx).get("chat.promptNavigator.currentPrompt"),
                                )
                            })
                            .h(px(ROW_HEIGHT))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .rounded(px(8.0))
                            .when(active, |row| row.bg(palette.selection))
                            .text_color(if active {
                                palette.selection_foreground
                            } else if selected {
                                palette.foreground
                            } else {
                                palette.foreground.opacity(0.7)
                            })
                            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                if let Some(panel) = this.prompt_panel.as_mut()
                                    && panel.highlighted != id
                                {
                                    panel.highlighted = id.clone();
                                    cx.notify();
                                }
                            }))
                            .child(
                                div()
                                    .truncate()
                                    .css_text(0.78125, 1.125, cx)
                                    .child(entry.preview),
                            ),
                        cx,
                        move |this, window, cx| this.select_prompt(entry.row, window, cx),
                    )
                    .test_support()
                }))
                .test_support()
                .into_any_element();
            div()
                .id("prompt-panel")
                .w(px(PANEL_WIDTH.min(self.chat_area_width(window) - 24.0)))
                .h(px(height))
                .flex()
                .flex_col()
                .rounded(px(12.0))
                .bg(palette.elevated)
                .shadow(elevation::floating(palette))
                .on_mouse_down_out(
                    cx.listener(|this, _, window, cx| this.close_prompt_panel(window, cx)),
                )
                .children(load_more)
                .child(list)
                .into_any_element()
        } else {
            let active = indicator_tick(current, total);
            on_activate(
                div()
                    .id("prompt-indicator")
                    .role(Role::Button)
                    .accessibility_id("prompt-indicator")
                    .aria_label(label.clone())
                    .aria_expanded(false)
                    .p(px(6.0))
                    .rounded(px(6.0))
                    .bg(palette.background.opacity(0.85))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .children((0..total.min(INDICATOR_TICKS)).map(|tick| {
                        div()
                            .w(px(TICK_WIDTH))
                            .h(px(TICK_HEIGHT))
                            .rounded_full()
                            .bg(palette.foreground.opacity(if tick == active {
                                1.0
                            } else {
                                INACTIVE_ALPHA
                            }))
                    })),
                cx,
                |this, window, cx| this.open_prompt_panel(false, window, cx),
            )
            .test_support()
            .into_any_element()
        };
        Some(
            div()
                .id("prompt-rail")
                .role(Role::Navigation)
                .accessibility_id("prompt-rail")
                .aria_label(label)
                .absolute()
                .top(px(CORNER_INSET))
                .inset_end(direction, px(CORNER_INSET))
                .on_hover(cx.listener(|this, hovered, window, cx| {
                    this.hover_prompt_panel(*hovered, window, cx)
                }))
                .child(content)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indicator_maps_the_whole_history_to_five_ticks() {
        assert_eq!(indicator_tick(0, 1), 0);
        assert_eq!(indicator_tick(0, 120), 0);
        assert_eq!(indicator_tick(60, 120), 2);
        assert_eq!(indicator_tick(119, 120), 4);
        assert_eq!(indicator_tick(2, 3), 2);
    }
}
