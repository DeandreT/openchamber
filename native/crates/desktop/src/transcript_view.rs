//! The transcript rows of the selected session.
//!
//! Each visited session keeps its own `ListState`, so its anchor message and
//! pixel offset survive switching away and back. Text deltas remeasure only
//! the affected row; tail following continues only while the reader is at
//! the end.

use gpui_kit::component::text::TextView;
use gpui_kit::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Role, StatefulInteractiveElement, Styled, TestSupportExt, Window, div, list,
    prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::transcript::{Message, PartContent, Role as MessageRole};

use crate::activation::on_activate;
use crate::chrome::{self, glyph};
use crate::connection::ConnectionFailure;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::{History, Workspace};

const MARKDOWN_SIZE: f32 = 0.875;
const META_SIZE: f32 = 0.781_25;

impl Workspace {
    pub(crate) fn render_transcript(
        &self,
        palette: &Palette,
        direction: Direction,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(open) = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
        else {
            return div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .text_color(palette.muted_foreground)
                .child(locale::strings(cx).get("chat.chatInput.placeholder.selectSession"))
                .into_any_element();
        };
        if let History::Failed(failure) = &open.history
            && open.transcript.is_empty()
        {
            let strings = locale::strings(cx);
            let description = match failure {
                ConnectionFailure::AuthenticationRequired
                | ConnectionFailure::AuthenticationRejected => {
                    strings.get("chat.container.sessionLoadError.authDescription")
                }
                _ => strings.get("chat.container.sessionLoadError.description"),
            };
            return div()
                .id("transcript-load-error")
                .role(Role::Alert)
                .accessibility_id("transcript-load-error")
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .gap_1()
                .items_center()
                .justify_center()
                .px_6()
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(strings.get("chat.container.sessionLoadError.title")),
                )
                .child(
                    div()
                        .max_w(px(420.0))
                        .text_center()
                        .text_size(rems(META_SIZE))
                        .text_color(palette.muted_foreground)
                        .child(description),
                )
                .into_any_element();
        }
        let following =
            open.list.is_following_tail() || open.list.is_scrolled_to_end().unwrap_or(true);
        let scroll_label = locale::strings(cx).get("chat.scrollToBottom.aria");
        let rail = self.render_prompt_rail(
            palette,
            direction,
            locale::strings(cx).get("chat.promptNavigator.aria"),
            window,
        );
        let rows = list(
            open.list.clone(),
            cx.processor(|this, index: usize, window, cx| this.render_message(index, window, cx)),
        )
        .size_full();
        div()
            .id("transcript")
            .role(Role::Log)
            .accessibility_id("transcript")
            .absolute()
            .inset_0()
            .padding_end(direction, px(chrome::NAVIGATION_GUTTER))
            .child(rows)
            .children(rail)
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
                                    .child(glyph("arrow-down-s", 16.0, palette.foreground)),
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = cx.global::<Palette>().clone();
        let Some(open) = self
            .selected
            .as_ref()
            .and_then(|id| self.open_sessions.get(id))
        else {
            return div().into_any_element();
        };
        let open_len = open.transcript.len();
        let Some(message) = open.transcript.message(index) else {
            return div().into_any_element();
        };
        let direction = locale::strings(cx).direction();
        let body = match message.info.role {
            MessageRole::User => Some(user_message(message, &palette)),
            MessageRole::Assistant => Some(
                div()
                    .flex()
                    .flex_col()
                    .child(assistant_message(message, &palette))
                    .children(
                        self.render_turn_footer(index, message, &palette, direction, window, cx),
                    )
                    .into_any_element(),
            ),
            MessageRole::Compaction | MessageRole::Shell => Some(notice_message(message, &palette)),
            // `timelineRoles.ts`: plumbing, and switches the composer already shows.
            MessageRole::Synthetic
            | MessageRole::System
            | MessageRole::Skill
            | MessageRole::LocationSwitched
            | MessageRole::Idle
            | MessageRole::AgentSwitched
            | MessageRole::ModelSwitched => None,
        };
        // The last row, shown or not, carries the tail spacer that keeps the
        // transcript's end clear of the docked composer.
        let tail = (index + 1 == open_len).then(|| self.composer_slot.get().tail_inset());
        let Some(body) = body else {
            return div()
                .when_some(tail, |this, tail| this.h(px(tail)))
                .into_any_element();
        };
        div()
            .id(ElementId::Name(message.info.id.as_str().to_owned().into()))
            .w_full()
            .pt(px(16.0))
            .when_some(tail, |this, tail| this.pb(px(tail)))
            .flex()
            .justify_center()
            .child(
                div()
                    .w_full()
                    .max_w(px(chrome::CHAT_COLUMN_MAX_WIDTH))
                    .px(chrome::chat_column_padding(window))
                    .child(body),
            )
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

/// A one-line record such as a switched agent or a finished turn.
fn notice_message(message: &Message, palette: &Palette) -> AnyElement {
    div()
        .text_size(rems(META_SIZE))
        .text_color(palette.muted_foreground)
        .when_some(message.info.notice.clone(), |this, notice| {
            this.child(notice)
        })
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
                    .child(glyph("terminal", 16.0, palette.muted_foreground))
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
            PartContent::Agent { name } => column.child(
                div()
                    .text_size(rems(META_SIZE))
                    .text_color(palette.muted_foreground)
                    .child(format!("@{name}")),
            ),
            PartContent::Unsupported { .. } => column,
        };
    }
    column.into_any_element()
}
