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
    Styled, TestSupportExt, Window, div, linear_color_stop, linear_gradient,
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
/// A custom Kit size whose text is 14 px; its editor insets text by 8 px
/// across and 2 px down.
const EDITOR_SIZE: f32 = 16.0;
const EDITOR_INSET_X: f32 = 8.0;
const EDITOR_INSET_Y: f32 = 2.0;
/// The editor's 14/21 px text, in rems of the 16 px root.
const TEXT_SIZE: f32 = 0.875;
const TEXT_LINE: f32 = 1.3125;

/// Below this editor width the composer shows the short placeholder
/// (`COMPACT_CHAT_PLACEHOLDER_MAX_WIDTH` in `ChatInput.tsx`).
const COMPACT_PLACEHOLDER_BELOW: f32 = 560.0;

impl Workspace {
    /// The composer box's width: the chat column, less the transcript's
    /// navigation gutter when a session is open, capped and padded as
    /// `.chat-input-column`.
    fn composer_width(&self, window: &Window) -> f32 {
        let mut column = f32::from(chrome::content_size(window).width) - context_panel::RAIL_WIDTH;
        if self.sidebar_open {
            column -= sidebar_view::SIDEBAR_WIDTH;
        }
        if self.work_status_open {
            column -= context_panel::PANEL_WIDTH;
        }
        if self.selected.is_some() {
            column -= chrome::NAVIGATION_GUTTER;
        }
        column.min(chrome::CHAT_COLUMN_MAX_WIDTH)
            - 2.0 * f32::from(chrome::chat_column_padding(window))
    }

    /// Shows the full placeholder, or the short one in a narrow composer.
    pub(crate) fn sync_composer_placeholder(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let compact = self.composer_width(window) < COMPACT_PLACEHOLDER_BELOW;
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
    pub(crate) fn render_composer(
        &self,
        palette: &Palette,
        direction: Direction,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
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
            .child(
                div()
                    .w_full()
                    .max_w(px(chrome::CHAT_COLUMN_MAX_WIDTH))
                    .px(chrome::chat_column_padding(window))
                    .child(self.render_composer_box(palette, direction, cx)),
            )
    }

    /// The composer box itself.
    pub(crate) fn render_composer_box(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> impl IntoElement {
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
                    .px(px(10.0))
                    .py(px(6.0))
                    .flex_row_from(direction)
                    .items_center()
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(6.0))
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
                            .flex_row_from(direction)
                            .items_center()
                            .justify_end()
                            .gap(px(12.0))
                            .child(action("composer-dictation", "mic", "chat.dictation.start"))
                            .child(send),
                    ),
            )
    }
}
