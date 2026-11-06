//! What the review flow draws: the review loop's row at the top of the
//! composer (`AutoReviewStrip.tsx`) and the review dialog
//! (`ReviewFlowDialog.tsx`).

use gpui_kit::component::WindowExt;
use gpui_kit::{
    AnyElement, Context, Hsla, InteractiveElement, IntoElement, MouseButton, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::auto_review::ResultTone;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

const ROW_HEIGHT: f32 = 36.0;
const DISPUTE_MAX_HEIGHT: f32 = 192.0;

fn result_color(tone: ResultTone, palette: &Palette) -> Hsla {
    match tone {
        ResultTone::Success => palette.status_success,
        ResultTone::Warning => palette.status_warning,
        ResultTone::Error => palette.status_error,
        ResultTone::Muted => palette.muted_foreground,
    }
}

impl Workspace {
    /// A quiet text button of the strip; its press keeps the editor's focus.
    fn strip_button(
        &self,
        id: &'static str,
        label: SharedString,
        color: Hsla,
        hover: Hsla,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        on_activate(
            div()
                .id(id)
                .accessibility_id(id)
                .role(Role::Button)
                .aria_label(label.clone())
                .h(px(28.0))
                .px(px(8.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .rounded(px(6.0))
                .cursor_pointer()
                .text_color(color)
                .hover(move |style| style.text_color(hover))
                .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                .child(div().css_text(0.875, 1.25, cx).child(label)),
            cx,
            action,
        )
        .test_support()
        .into_any_element()
    }

    /// The selected session's review loop: while it runs, what it waits for,
    /// Open and Stop; once it ended, the result until dismissed, a dispute
    /// unfolding in place.
    pub(crate) fn render_auto_review_strip(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let run = self.selected_auto_review()?;
        let strings = locale::strings(cx);
        let open = self.strip_button(
            "auto-review-open",
            strings.get("chat.autoReview.actions.open"),
            palette.muted_foreground,
            palette.foreground,
            cx,
            |this, window, cx| this.open_auto_review_session(window, cx),
        );
        let row = || {
            div()
                .min_h(px(ROW_HEIGHT))
                .min_w_0()
                .padding_start(direction, px(12.0))
                .padding_end(direction, px(6.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
        };
        let strip = div()
            .id("auto-review-strip")
            .accessibility_id("auto-review-strip")
            .role(Role::Status)
            .w_full()
            .border_b_1()
            .border_color(palette.border.opacity(0.6))
            .flex()
            .flex_col();
        let Some(result) = run.status.result() else {
            let label = format!(
                "{} · {}",
                strings.get("chat.autoReview.title"),
                strings.get(run.step.label_key())
            );
            return Some(
                strip
                    .aria_label(label)
                    .child(
                        row()
                            .child(glyph("loader-4", 14.0, palette.muted_foreground))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .css_text(0.875, 1.25, cx)
                                    .text_color(palette.foreground)
                                    .child(strings.get("chat.autoReview.title"))
                                    .child(div().text_color(palette.muted_foreground).child(
                                        format!(" · {}", strings.get(run.step.label_key())),
                                    )),
                            )
                            .child(open)
                            .child(self.strip_button(
                                "auto-review-stop",
                                strings.get("chat.autoReview.actions.stop"),
                                palette.status_error.opacity(0.8),
                                palette.status_error,
                                cx,
                                |this, window, cx| this.remove_auto_review(window, cx),
                            )),
                    )
                    .test_support()
                    .into_any_element(),
            );
        };
        let label: SharedString = match &run.error {
            Some(error) => format!("{} · {error}", strings.get(result.label_key)).into(),
            None => strings.get(result.label_key),
        };
        let unfolded = run.dispute.is_some()
            && self.reviews.unfolded.as_ref()
                == Some(&(run.original_session.clone(), run.started_at));
        let text = div()
            .flex_1()
            .min_w_0()
            .truncate()
            .css_text(0.875, 1.25, cx)
            .text_color(palette.foreground)
            .child(strings.get(result.label_key))
            .when_some(run.error.clone(), |this, error| {
                this.child(
                    div()
                        .text_color(palette.muted_foreground)
                        .child(format!(" · {error}")),
                )
            });
        let summary = if run.dispute.is_some() {
            on_activate(
                div()
                    .id("auto-review-dispute-toggle")
                    .accessibility_id("auto-review-dispute-toggle")
                    .role(Role::Button)
                    .aria_label(label.clone())
                    .aria_expanded(unfolded)
                    .flex_1()
                    .min_w_0()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .child(text)
                    .child(glyph(
                        if unfolded {
                            "arrow-up-s"
                        } else {
                            "arrow-down-s"
                        },
                        16.0,
                        palette.muted_foreground,
                    )),
                cx,
                |this, _, cx| this.toggle_auto_review_dispute(cx),
            )
            .test_support()
            .into_any_element()
        } else {
            text.into_any_element()
        };
        Some(
            strip
                .aria_label(label)
                .child(
                    row()
                        .child(glyph(result.icon, 14.0, result_color(result.tone, palette)))
                        .child(summary)
                        .child(open)
                        .child(self.strip_button(
                            "auto-review-dismiss",
                            strings.get("chat.autoReview.actions.dismiss"),
                            palette.muted_foreground,
                            palette.foreground,
                            cx,
                            |this, window, cx| this.remove_auto_review(window, cx),
                        )),
                )
                .when(unfolded, |this| {
                    this.child(
                        div()
                            .id("auto-review-dispute")
                            .accessibility_id("auto-review-dispute")
                            .max_h(px(DISPUTE_MAX_HEIGHT))
                            .overflow_y_scroll()
                            .padding_start(direction, px(32.0))
                            .padding_end(direction, px(12.0))
                            .pb(px(8.0))
                            .css_text(0.875, 1.25, cx)
                            .text_color(palette.muted_foreground)
                            .child(run.dispute.clone().unwrap_or_default())
                            .test_support(),
                    )
                })
                .test_support()
                .into_any_element(),
        )
    }

    /// Opens the window dialog that renders the review dialog's state.
    pub(crate) fn show_review_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = workspace.upgrade() else {
                return dialog;
            };
            let palette = cx.global::<Palette>().clone();
            let strings = locale::strings(cx).clone();
            let direction = strings.direction();
            let Some(open) = this.read(cx).reviews.dialog.clone() else {
                return dialog;
            };
            let submitting = open.submitting;
            let can_confirm = open.model.is_some();
            let checkbox = |id: &'static str, label: SharedString, checked: bool| {
                div()
                    .id(id)
                    .role(Role::CheckBox)
                    .accessibility_id(id)
                    .aria_label(label.clone())
                    .aria_toggled(if checked {
                        gpui_kit::Toggled::True
                    } else {
                        gpui_kit::Toggled::False
                    })
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(8.0))
                    .when(!submitting, |this| this.cursor_pointer())
                    .when(submitting, |this| this.opacity(0.5))
                    .child(glyph(
                        if checked {
                            "checkbox"
                        } else {
                            "checkbox-blank"
                        },
                        16.0,
                        if checked {
                            palette.primary
                        } else {
                            palette.muted_foreground
                        },
                    ))
                    .child(
                        div()
                            .text_size(gpui_kit::rems(0.8125))
                            .text_color(palette.foreground)
                            .child(label),
                    )
                    .test_support()
            };
            let field = |label_key: &str, value: Option<String>| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(gpui_kit::rems(0.75))
                            .text_color(palette.muted_foreground)
                            .child(strings.get(label_key)),
                    )
                    .child(
                        div()
                            .text_size(gpui_kit::rems(0.8125))
                            .text_color(palette.foreground)
                            .child(value.unwrap_or_default()),
                    )
            };
            let on_handoff = workspace.clone();
            let on_auto = workspace.clone();
            let on_cancel = workspace.clone();
            let on_start = workspace.clone();
            let on_close = workspace.clone();
            let button = |id: &'static str,
                          label: SharedString,
                          fill: Option<Hsla>,
                          text: Hsla,
                          enabled: bool| {
                div()
                    .id(id)
                    .role(Role::Button)
                    .accessibility_id(id)
                    .aria_label(label.clone())
                    .h(px(28.0))
                    .px(px(10.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .text_color(text)
                    .when_some(fill, |this, fill| this.bg(fill))
                    .when(enabled, |this| this.cursor_pointer())
                    .when(!enabled, |this| {
                        this.opacity(0.5)
                            .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
                    })
                    .child(div().text_size(gpui_kit::rems(0.8125)).child(label))
                    .test_support()
            };
            let model = open
                .model
                .as_ref()
                .map(|model| format!("{}/{}", model.provider_id, model.model_id));
            let variant = open.model.as_ref().and_then(|model| model.variant.clone());
            dialog
                .w(px(448.0))
                .on_close(move |_, _, cx| {
                    let _ = on_close.update(cx, |this, cx| {
                        if !this
                            .reviews
                            .dialog
                            .as_ref()
                            .is_some_and(|dialog| dialog.submitting)
                        {
                            this.reviews.dialog = None;
                        }
                        cx.notify();
                    });
                })
                .title(strings.get("diffView.reviewDialog.title"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(16.0))
                        .child(
                            div()
                                .text_size(gpui_kit::rems(0.8125))
                                .text_color(palette.muted_foreground)
                                .child(strings.get("diffView.reviewDialog.description")),
                        )
                        .child(
                            div()
                                .px(px(12.0))
                                .py(px(8.0))
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(palette.status_info.opacity(0.35))
                                .bg(palette.status_info.opacity(0.1))
                                .text_size(gpui_kit::rems(0.75))
                                .text_color(palette.foreground)
                                .child(strings.get("diffView.reviewDialog.info")),
                        )
                        .child(
                            checkbox(
                                "review-dialog-handoff",
                                strings.get("diffView.reviewDialog.generateHandoff"),
                                open.generate_handoff,
                            )
                            .on_click(move |_, _, cx| {
                                let _ = on_handoff
                                    .update(cx, |this, cx| this.toggle_review_handoff(cx));
                            }),
                        )
                        .child(
                            checkbox(
                                "review-dialog-auto",
                                strings.get("diffView.reviewDialog.autoReview"),
                                open.auto_review,
                            )
                            .on_click(move |_, _, cx| {
                                let _ = on_auto.update(cx, |this, cx| this.toggle_review_auto(cx));
                            }),
                        )
                        .child(field("chat.modelControls.model", model))
                        .child(field(
                            "sessions.scheduledTasks.editor.thinkingLevel.label",
                            variant,
                        ))
                        .child(field(
                            "sessions.scheduledTasks.editor.agent.label",
                            open.agent.clone(),
                        )),
                )
                .footer(
                    div()
                        .w_full()
                        .flex_row_from(direction)
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            button(
                                "review-dialog-cancel",
                                strings.get("diffView.reviewDialog.actions.cancel"),
                                None,
                                palette.foreground,
                                !submitting,
                            )
                            .when(!submitting, |this| {
                                this.on_click(move |_, window, cx| {
                                    let _ = on_cancel.update(cx, |this, cx| {
                                        this.close_review_dialog(window, cx)
                                    });
                                })
                            }),
                        )
                        .child(
                            button(
                                "review-dialog-start",
                                strings.get(if submitting {
                                    "diffView.reviewDialog.actions.starting"
                                } else {
                                    "diffView.reviewDialog.actions.start"
                                }),
                                Some(palette.primary),
                                palette.primary_foreground,
                                can_confirm && !submitting,
                            )
                            .when(can_confirm && !submitting, |this| {
                                this.on_click(move |_, window, cx| {
                                    let _ = on_start
                                        .update(cx, |this, cx| this.start_review(window, cx));
                                })
                            }),
                        ),
                )
        });
        cx.notify();
    }
}
