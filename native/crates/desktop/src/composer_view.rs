//! The retained multiline composer: in the startup view, and anchored below
//! the transcript of a selected session.
//!
//! The box mirrors the web client's desktop composer (`.oc-surface-floating
//! .oc-glass-floating .oc-composer-sheen`): an attachment strip, the editor
//! and a toolbar of composer actions with send at the end.

use gpui_kit::component::input::Textarea;
use gpui_kit::component::{Sizable, Size};
use gpui_kit::{
    Context, InteractiveElement, IntoElement, ParentElement, Role, StatefulInteractiveElement,
    Styled, TestSupportExt, Window, canvas, div, linear_color_stop, linear_gradient,
    prelude::FluentBuilder, px, rems,
};

use crate::activation::on_activate;
use crate::chrome::{self, ButtonTone, IconButton, glyph};
use crate::elevation;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;
use crate::{context_panel, sidebar_view};

const BOX_RADIUS: f32 = 12.0;
const EDITOR_MIN_HEIGHT: f32 = 52.0;
const TOOLBAR_HEIGHT: f32 = 44.0;
const ACTION_SIZE: f32 = 24.0;
const ACTION_GLYPH: f32 = 18.0;
/// The toolbar's `px-2.5`, the start actions' and the groups' `gap-x-1.5`,
/// and the end controls' `md:gap-x-3`.
const TOOLBAR_PADDING_X: f32 = 10.0;
const START_GAP: f32 = 6.0;
const END_GAP: f32 = 12.0;
/// Attach, focus mode, permissions and goal.
const START_ACTIONS: f32 = 4.0;
/// A custom Kit size whose text is 14 px; its editor insets text by 8 px
/// across and 2 px down.
const EDITOR_SIZE: f32 = 16.0;
const EDITOR_INSET_X: f32 = 8.0;
const EDITOR_INSET_Y: f32 = 2.0;
/// The editor's 14/21 px text, in rems of the 16 px root.
const TEXT_SIZE: f32 = 0.875;
const TEXT_LINE: f32 = 1.3125;

/// The composer box's width around the editor's text: the web editor's
/// 12 px on each side plus its content's 1 px start padding.
const EDITOR_TEXT_INSET: f32 = 25.0;

/// `composerTailInset.ts`: the gap between the transcript's last row and the
/// resting composer, how far the composer grows before it pushes the
/// transcript, and the slot height assumed before one is measured.
const TAIL_GAP: f32 = 80.0;
const GROWTH_ALLOWANCE: f32 = 88.0;
const DEFAULT_SLOT_HEIGHT: f32 = 128.0;

/// The docked composer's slot as last measured.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ComposerSlot {
    /// The smallest height measured: the composer at rest.
    resting: Option<f32>,
    height: Option<f32>,
}

impl ComposerSlot {
    /// Records a measured height; a zero height is a slot not laid out yet.
    fn measured(self, height: f32) -> Self {
        if height <= 0.0 {
            return self;
        }
        Self {
            resting: Some(self.resting.map_or(height, |resting| resting.min(height))),
            height: Some(height),
        }
    }

    /// How much of the transcript's end the composer reserves
    /// (`resolveComposerTailInset`).
    pub(crate) fn tail_inset(self) -> f32 {
        let resting = self.resting.unwrap_or(DEFAULT_SLOT_HEIGHT);
        let height = self.height.unwrap_or(resting);
        (resting + TAIL_GAP).max(height + TAIL_GAP - GROWTH_ALLOWANCE)
    }
}

impl Workspace {
    /// The chat area's width: the content between the sidebar and the work
    /// status panel and rail.
    pub(crate) fn chat_area_width(&self, window: &Window) -> f32 {
        let mut area = f32::from(chrome::content_size(window).width) - context_panel::RAIL_WIDTH;
        if self.sidebar_open {
            area -= sidebar_view::SIDEBAR_WIDTH;
        }
        if self.work_status_inline(window) {
            area -= context_panel::PANEL_WIDTH;
        }
        area
    }

    /// The composer box's width: the chat column, less the transcript's
    /// navigation gutter when a session is open, capped and padded as
    /// `.chat-input-column`.
    pub(crate) fn composer_width(&self, window: &Window) -> f32 {
        let mut column = self.chat_area_width(window);
        if self.selected.is_some() {
            column -= chrome::NAVIGATION_GUTTER;
        }
        column.min(chrome::CHAT_COLUMN_MAX_WIDTH)
            - 2.0 * f32::from(chrome::chat_column_padding(window))
    }

    /// Shows the full placeholder, or the short one when the full one would
    /// wrap (`usePlaceholderFitsOneLine` in the web composer).
    pub(crate) fn sync_composer_placeholder(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let full = locale::strings(cx).get("chat.chatInput.placeholder.chat");
        let available = self.composer_width(window) - EDITOR_TEXT_INSET;
        let compact = editor_text_width(&full, window) > available;
        if self.composer_placeholder_compact == Some(compact) {
            return;
        }
        self.composer_placeholder_compact = Some(compact);
        let placeholder = locale::strings(cx).get(if compact {
            "chat.chatInput.placeholder.chatCompact"
        } else {
            "chat.chatInput.placeholder.chat"
        });
        self.composer.update(cx, |composer, cx| {
            composer.set_placeholder(placeholder, window, cx);
        });
    }

    /// The composer docked over the bottom of a selected session's
    /// transcript, in the chat column beside the prompt navigation gutter.
    /// Its slot is measured every frame; a new tail inset remeasures the
    /// transcript's last row, which carries the spacer.
    pub(crate) fn render_composer(
        &self,
        palette: &Palette,
        direction: Direction,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let slot = self.composer_slot.clone();
        let list = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
            .map(|open| open.list.clone());
        let measure = canvas(
            move |bounds, window, _| {
                let previous = slot.get();
                let next = previous.measured(f32::from(bounds.size.height));
                slot.set(next);
                if next.tail_inset() != previous.tail_inset()
                    && let Some(list) = &list
                {
                    let count = list.item_count();
                    if count > 0 {
                        list.remeasure_items(count - 1..count);
                    }
                    window.refresh();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        div()
            .id("composer-region")
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .padding_end(direction, px(chrome::NAVIGATION_GUTTER))
            .pb(px(16.0))
            .flex()
            .justify_center()
            .child(measure)
            .child(
                div()
                    .w_full()
                    .max_w(px(chrome::CHAT_COLUMN_MAX_WIDTH))
                    .px(chrome::chat_column_padding(window))
                    .child(self.render_composer_box(palette, direction, window, cx)),
            )
    }

    /// The composer box itself.
    pub(crate) fn render_composer_box(
        &self,
        palette: &Palette,
        direction: Direction,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        // The width the effort, model and agent controls get: the web's
        // `@container/model-controls` hides their labels by it.
        let controls_width = self.composer_width(window)
            - 2.0 * TOOLBAR_PADDING_X
            - START_ACTIONS * ACTION_SIZE
            - (START_ACTIONS - 1.0) * START_GAP
            - START_GAP
            - 2.0 * (END_GAP + ACTION_SIZE);
        let streaming = self
            .selected
            .as_ref()
            .is_some_and(|id| self.is_streaming(id));
        let empty = self.composer.read(cx).value().trim().is_empty();
        // A server connection stays read-only until prompts have their error
        // and reconciliation paths.
        let can_send = !empty && self.is_synthetic() && self.selected.is_some();
        let strings = locale::strings(cx);
        let (label, icon) = if streaming {
            (
                strings.get("chat.chatInput.actions.stopGeneratingAria"),
                "stop",
            )
        } else {
            (
                strings.get("chat.chatInput.actions.sendMessageAria"),
                "send-plane-2",
            )
        };
        let action = |id: &'static str, icon: &'static str, key: &str| {
            IconButton::new(id, icon, strings.get(key))
                .size(ACTION_SIZE, ACTION_GLYPH)
                .tone(ButtonTone::Active)
                .render(palette, cx, |_, _, _| {})
        };
        let send = on_activate(
            div()
                .id("composer-submit")
                .role(Role::Button)
                .accessibility_id("composer-submit")
                .aria_label(label)
                .size(px(ACTION_SIZE))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .when(streaming || can_send, |this| this.cursor_pointer())
                .when(!streaming && !can_send, |this| this.opacity(0.3))
                .child(glyph(icon, 16.0, palette.foreground)),
            cx,
            move |this, window, cx| {
                if streaming {
                    this.stop(cx);
                } else {
                    this.submit(window, cx);
                }
            },
        )
        .test_support();
        div()
            .id("composer")
            .role(Role::Group)
            .accessibility_id("composer")
            .relative()
            .w_full()
            .rounded(px(BOX_RADIUS))
            .bg(palette.elevated)
            .text_color(palette.elevated_foreground)
            .shadow(elevation::composer(palette))
            .flex()
            .flex_col()
            .when_some(elevation::composer_sheen(palette), |this, sheen| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(px(BOX_RADIUS))
                        .bg(linear_gradient(
                            180.0,
                            linear_color_stop(sheen, 0.0),
                            linear_color_stop(sheen.opacity(0.0), 0.5),
                        )),
                )
            })
            .child(div().h(px(4.0)))
            .child(
                // The editor insets its text by the size's padding; the
                // rest of the web editor's `16px 12px 8px` (plus its line's
                // 1 px start padding) sits around it.
                div()
                    .min_h(px(EDITOR_MIN_HEIGHT))
                    .pt(px(16.0 - EDITOR_INSET_Y))
                    .padding_start(direction, px(13.0 - EDITOR_INSET_X))
                    .padding_end(direction, px(12.0 - EDITOR_INSET_X))
                    .pb(px(8.0 - EDITOR_INSET_Y))
                    .child(
                        Textarea::new(&self.composer)
                            .appearance(false)
                            .bordered(false)
                            .with_size(Size::Size(px(EDITOR_SIZE)))
                            .text_size(rems(TEXT_SIZE))
                            .line_height(rems(TEXT_LINE))
                            .aria_label(strings.get("chat.chatInput.placeholder.chat")),
                    ),
            )
            .child(
                div()
                    .h(px(TOOLBAR_HEIGHT))
                    .flex_shrink_0()
                    .px(px(TOOLBAR_PADDING_X))
                    .py(px(6.0))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(START_GAP))
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(START_GAP))
                            .child(action(
                                "composer-attach",
                                "add-circle",
                                "chat.chatInput.actions.addAttachment",
                            ))
                            .child(action(
                                "composer-focus-mode",
                                "fullscreen",
                                "chat.chatInput.focusMode.toggleAria",
                            ))
                            .child(action(
                                "composer-permissions",
                                "shield-user",
                                "chat.chatInput.permissionMode.ask",
                            ))
                            .child(action(
                                "composer-goal",
                                "target",
                                "chat.goal.button.armAria",
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex_row_from(direction)
                            .items_center()
                            .justify_end()
                            .gap(px(END_GAP))
                            // Synthetic data has no catalogs to select from.
                            .when(!self.is_synthetic(), |this| {
                                this.child(self.render_model_controls(
                                    palette,
                                    direction,
                                    controls_width,
                                    cx,
                                ))
                            })
                            .child(action("composer-dictation", "mic", "chat.dictation.start"))
                            .child(send),
                    ),
            )
    }
}

/// The width of `text` in the editor's font.
fn editor_text_width(text: &str, window: &Window) -> f32 {
    let run = window.text_style().to_run(text.len());
    let size = rems(TEXT_SIZE).to_pixels(window.rem_size());
    f32::from(
        window
            .text_system()
            .layout_line(text, size, &[run], None)
            .width,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_keeps_the_resting_composer_and_a_gap() {
        let unmeasured = ComposerSlot::default();
        assert_eq!(
            unmeasured.tail_inset(),
            208.0,
            "the web's default before a measurement"
        );
        let resting = unmeasured.measured(0.0).measured(115.0);
        assert_eq!(resting.tail_inset(), 195.0);
        // Growth takes the gap first, then pushes the transcript.
        assert_eq!(resting.measured(180.0).tail_inset(), 195.0);
        assert_eq!(resting.measured(250.0).tail_inset(), 242.0);
        assert_eq!(resting.measured(250.0).measured(115.0).tail_inset(), 195.0);
    }
}
