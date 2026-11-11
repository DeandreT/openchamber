//! What an opened `question` row shows (the question branch of
//! `ToolExpandedContent` in `ToolPart.tsx`): each question with the answer
//! the user gave, or, before an answer and when the result is in another
//! shape, the questions as they were asked with their options.

use gpui_kit::{
    AnyElement, InteractiveElement, IntoElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, div, prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::ids::PartId;
use openchamber_native_core::tool_shells::question::{AskedQuestion, QuestionAnswer};

use super::{MICRO_SIZE, tile_inset};
use crate::locale::Direction;
use crate::logical::LogicalStyledExt;
use crate::markdown_view::{self, Author};
use crate::theme::Palette;
use crate::transcript_view::META_SIZE;

/// The questions with the answers the result reports.
pub(super) fn answered(part: &PartId, answers: &[QuestionAnswer], palette: &Palette) -> AnyElement {
    let id: SharedString = format!("tool:{part}:answers").into();
    let mut list = div()
        .id(gpui_kit::ElementId::Name(id.clone()))
        .role(Role::Group)
        .accessibility_id(id)
        .flex()
        .flex_col()
        .gap_2();
    for (index, pair) in answers.iter().enumerate() {
        list = list.child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div().text_color(palette.muted_foreground).child(
                        markdown_view::markdown(
                            &format!("tool:{part}:answered:{index}"),
                            &pair.question,
                            palette,
                            Author::Assistant,
                        )
                        .text_size(rems(MICRO_SIZE)),
                    ),
                )
                .child(
                    div()
                        .text_size(rems(META_SIZE))
                        .text_color(palette.foreground)
                        .whitespace_normal()
                        .child(pair.answer.clone()),
                ),
        );
    }
    list.test_support().into_any_element()
}

/// The questions as the call asked them: header, text and option labels.
pub(super) fn asked(
    part: &PartId,
    questions: &[AskedQuestion],
    palette: &Palette,
    direction: Direction,
) -> AnyElement {
    let id: SharedString = format!("tool:{part}:questions").into();
    let mut list = div()
        .id(gpui_kit::ElementId::Name(id.clone()))
        .role(Role::Group)
        .accessibility_id(id)
        .flex()
        .flex_col()
        .gap_2();
    for (index, question) in questions.iter().enumerate() {
        let options = (!question.options.is_empty()).then(|| {
            let mut chips = div()
                .mt(px(2.0))
                .flex()
                .flex_row_from(direction)
                .flex_wrap()
                .gap_1();
            for label in &question.options {
                chips = chips.child(
                    div()
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(4.0))
                        .bg(tile_inset(palette))
                        .text_size(rems(MICRO_SIZE))
                        .text_color(palette.muted_foreground)
                        .child(label.clone()),
                );
            }
            chips
        });
        list = list.child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .when_some(question.header.clone(), |this, header| {
                    this.child(
                        div()
                            .text_size(rems(MICRO_SIZE))
                            .text_color(palette.muted_foreground)
                            .child(header),
                    )
                })
                .child(
                    markdown_view::markdown(
                        &format!("tool:{part}:asked:{index}"),
                        &question.question,
                        palette,
                        Author::User,
                    )
                    .text_size(rems(META_SIZE)),
                )
                .children(options),
        );
    }
    list.test_support().into_any_element()
}
