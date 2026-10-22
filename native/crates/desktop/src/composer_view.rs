//! The retained multiline composer anchored below the transcript.

use gpui_kit::component::input::Textarea;
use gpui_kit::{
    Context, InteractiveElement, IntoElement, ParentElement, Role, StatefulInteractiveElement,
    Styled, TestSupportExt, div, prelude::FluentBuilder, px,
};

use crate::locale;
use crate::sidebar_view::icon_element;
use crate::theme::Palette;
use crate::workspace::Workspace;

const COLUMN_MAX_WIDTH: f32 = 768.0;

impl Workspace {
    pub(crate) fn render_composer(
        &self,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let streaming = self
            .selected
            .as_ref()
            .is_some_and(|id| self.is_streaming(id));
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
        div()
            .id("composer-region")
            .flex_shrink_0()
            .px_4()
            .pb_3()
            .pt_2()
            .flex()
            .justify_center()
            .child(
                div()
                    .id("composer")
                    .w_full()
                    .max_w(px(COLUMN_MAX_WIDTH))
                    .rounded(px(12.0))
                    .bg(palette.elevated)
                    .text_color(palette.elevated_foreground)
                    .border_1()
                    .border_color(palette.border)
                    .px_3()
                    .pt_2()
                    .pb_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        Textarea::new(&self.composer)
                            .appearance(false)
                            .bordered(false)
                            .aria_label(locale::strings(cx).get("chat.chatInput.placeholder.chat")),
                    )
                    .child(
                        div().flex().justify_end().child(
                            div()
                                .id("composer-submit")
                                .role(Role::Button)
                                .accessibility_id("composer-submit")
                                .aria_label(label)
                                .size(px(28.0))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .when(streaming, |this| this.bg(palette.active))
                                .when(!streaming, |this| {
                                    this.bg(palette.primary)
                                        .hover(|style| style.bg(palette.primary_hover))
                                })
                                .child(icon_element(
                                    icon,
                                    palette,
                                    if streaming {
                                        palette.foreground
                                    } else {
                                        palette.primary_foreground
                                    },
                                ))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if streaming {
                                        this.stop(cx);
                                    } else {
                                        this.submit(window, cx);
                                    }
                                }))
                                .test_support(),
                        ),
                    ),
            )
    }
}
