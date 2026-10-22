//! The conversation header and transcript rows.
//!
//! Each visited session keeps its own `ListState`, so its anchor message and
//! pixel offset survive switching away and back. Text deltas remeasure only
//! the affected row; tail following continues only while the reader is at
//! the end.

use gpui_kit::component::text::TextView;
use gpui_kit::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Role, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div, list,
    prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::transcript::{Message, PartContent, Role as MessageRole};

use crate::activation::on_activate;
use crate::locale;
use crate::sidebar_view::icon_element;
use crate::theme::Palette;
use crate::workspace::Workspace;

const HEADER_HEIGHT: f32 = 44.0;
/// The readable column width of the chat.
const COLUMN_MAX_WIDTH: f32 = 768.0;
const MARKDOWN_SIZE: f32 = 0.875;
const META_SIZE: f32 = 0.781_25;

impl Workspace {
    pub(crate) fn render_header(
        &self,
        palette: &Palette,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title: SharedString = self
            .selected_session()
            .map(|session| session.title.clone().into())
            .unwrap_or_default();
        div()
            .id("conversation-header")
            .h(px(HEADER_HEIGHT))
            .flex_shrink_0()
            .px_4()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(palette.divider)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(rems(MARKDOWN_SIZE))
                    .font_weight(FontWeight::MEDIUM)
                    .child(title),
            )
    }

    pub(crate) fn render_transcript(
        &self,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(open) = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
        else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(palette.muted_foreground)
                .child(locale::strings(cx).get("chat.chatInput.placeholder.selectSession"))
                .into_any_element();
        };
        let following =
            open.list.is_following_tail() || open.list.is_scrolled_to_end().unwrap_or(true);
        let scroll_label = locale::strings(cx).get("chat.scrollToBottom.aria");
        let rows = list(
            open.list.clone(),
            cx.processor(|this, index: usize, window, cx| this.render_message(index, window, cx)),
        )
        .size_full();
        div()
            .id("transcript")
            .role(Role::Log)
            .accessibility_id("transcript")
            .relative()
            .flex_1()
            .min_h_0()
            .child(rows)
            .when(!following, |this| {
                this.child(
                    div()
                        .absolute()
                        .bottom_3()
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            on_activate(
                                div()
                                    .id("scroll-to-bottom")
                                    .role(Role::Button)
                                    .accessibility_id("scroll-to-bottom")
                                    .aria_label(scroll_label)
                                    .size(px(32.0))
                                    .rounded_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(palette.elevated)
                                    .border_1()
                                    .border_color(palette.border)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(palette.hover))
                                    .child(icon_element(
                                        "arrow-down-s",
                                        palette,
                                        palette.foreground,
                                    )),
                                cx,
                                |this, _, cx| this.scroll_to_tail(cx),
                            )
                            .test_support(),
                        ),
                )
            })
            .test_support()
            .into_any_element()
    }

    fn render_message(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = cx.global::<Palette>().clone();
        let Some(message) = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
            .and_then(|open| open.transcript.message(index))
        else {
            return div().into_any_element();
        };
        let body = match message.info.role {
            MessageRole::User => user_message(message, &palette),
            MessageRole::Assistant => assistant_message(message, &palette),
        };
        div()
            .id(ElementId::Name(message.info.id.as_str().to_owned().into()))
            .w_full()
            .px_4()
            .pt_4()
            .flex()
            .justify_center()
            .child(div().w_full().max_w(px(COLUMN_MAX_WIDTH)).child(body))
            .into_any_element()
    }
}

fn markdown(id: &str, text: &str) -> TextView {
    TextView::markdown(ElementId::Name(id.to_owned().into()), text.to_owned())
        .selectable(true)
        .text_size(rems(MARKDOWN_SIZE))
}

fn user_message(message: &Message, palette: &Palette) -> AnyElement {
    let text = message.plain_text();
    let part_id = message
        .parts
        .first()
        .map(|part| part.id.as_str().to_owned())
        .unwrap_or_default();
    div()
        .flex()
        .justify_end()
        .child(
            div()
                .max_w(gpui_kit::relative(0.85))
                .px_3()
                .py_2()
                .rounded(px(12.0))
                .bg(palette.user_message_background)
                .child(markdown(&part_id, &text)),
        )
        .into_any_element()
}

fn assistant_message(message: &Message, palette: &Palette) -> AnyElement {
    let mut column = div().flex().flex_col().gap_2();
    for part in &message.parts {
        column = match &part.content {
            PartContent::Text { text } => column.child(markdown(part.id.as_str(), text)),
            PartContent::Reasoning { text } => column.child(
                div()
                    .text_size(rems(META_SIZE))
                    .italic()
                    .text_color(palette.muted_foreground)
                    .child(text.clone()),
            ),
            PartContent::Tool {
                tool,
                title,
                output,
                ..
            } => column.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(rems(META_SIZE))
                    .text_color(palette.muted_foreground)
                    .child(icon_element("terminal", palette, palette.muted_foreground))
                    .child(div().font_weight(FontWeight::MEDIUM).child(tool.clone()))
                    .when_some(title.clone(), |this, title| {
                        this.child(div().truncate().child(title))
                    })
                    .when_some(output.clone(), |this, output| this.child(output)),
            ),
            PartContent::File { filename, .. } => column.child(
                div()
                    .text_size(rems(META_SIZE))
                    .text_color(palette.muted_foreground)
                    .child(filename.clone()),
            ),
            PartContent::Unsupported { .. } => column,
        };
    }
    column.into_any_element()
}
