//! What the session goal draws: the composer's target button and objective
//! counter (`SessionGoalButton.tsx`), the goal row at the top of the composer
//! (`SessionGoalRow.tsx`) and the create and manage dialog
//! (`SessionGoalDialog.tsx`).

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::{
    AnyElement, Context, Hsla, InteractiveElement, IntoElement, MouseButton, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div,
    prelude::FluentBuilder, px,
};
use openchamber_native_core::session::LiveStatus;
use openchamber_native_core::session_goal::{
    self, GoalTone, GoalUsage, OBJECTIVE_CHAR_LIMIT, SessionGoal,
};
use openchamber_native_transport::session_goal::GoalStatusChange;

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

const ACTION_SIZE: f32 = 24.0;
const ACTION_GLYPH: f32 = 16.0;
const ROW_HEIGHT: f32 = 36.0;

/// `sessionGoalStatusColor`.
pub(crate) fn tone_color(tone: GoalTone, palette: &Palette) -> Hsla {
    match tone {
        GoalTone::Info => palette.status_info,
        GoalTone::Muted => palette.muted_foreground,
        GoalTone::Warning => palette.status_warning,
        GoalTone::Success => palette.status_success,
    }
}

fn usage_text(usage: GoalUsage, strings: &Strings) -> SharedString {
    match usage {
        GoalUsage::Tokens { used } => strings.format(
            "chat.goal.usage.tokens",
            &[("used", &session_goal::format_tokens(used))],
        ),
        GoalUsage::TokensWithBudget { used, budget } => strings.format(
            "chat.goal.usage.tokensWithBudget",
            &[
                ("used", &session_goal::format_tokens(used)),
                ("budget", &session_goal::format_tokens(budget)),
            ],
        ),
    }
}

impl Workspace {
    /// The target button: arms the next send, or opens the goal's dialog.
    /// Hidden while goals are off.
    pub(crate) fn render_goal_button(
        &self,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.goals_enabled() {
            return None;
        }
        let strings = locale::strings(cx);
        let goal = self.selected_goal();
        let armed = self.goals.armed;
        let cannot_check = goal.is_none() && !self.goal_check_available();
        let label = if goal.is_some() {
            strings.get("chat.goal.button.manageAria")
        } else if cannot_check {
            strings.get("chat.goal.button.noSmallModel")
        } else if armed {
            strings.get("chat.goal.button.disarmAria")
        } else {
            strings.get("chat.goal.button.armAria")
        };
        let color = match goal {
            Some(goal) => tone_color(goal.status.tone(), palette),
            None if armed => palette.status_info,
            None => palette.foreground,
        };
        let engaged = armed || goal.is_some_and(SessionGoal::is_live);
        let filled = engaged || goal.is_some();
        let arm_only = goal.is_none();
        let tooltip = label.clone();
        Some(
            on_activate(
                div()
                    .id("composer-goal")
                    .role(Role::Button)
                    .accessibility_id("composer-goal")
                    .aria_label(label)
                    .aria_toggled(if engaged {
                        gpui_kit::Toggled::True
                    } else {
                        gpui_kit::Toggled::False
                    })
                    .when(cannot_check, |this| {
                        this.opacity(0.5)
                            .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
                    })
                    .size(px(ACTION_SIZE))
                    .flex_shrink_0()
                    .rounded(px(crate::chrome::CONTROL_RADIUS))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    // Arming happens mid-typing: that press keeps the
                    // editor's focus. Opening the dialog moves it as usual.
                    .when(arm_only, |this| {
                        this.on_mouse_down(MouseButton::Left, |_, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                        })
                    })
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tooltip.clone())
                            .build(window, cx)
                    })
                    .child(glyph(
                        if filled { "target-fill" } else { "target" },
                        ACTION_GLYPH,
                        color,
                    )),
                cx,
                |this, window, cx| this.press_goal_button(window, cx),
            )
            .test_support()
            .into_any_element(),
        )
    }

    /// `SessionGoalObjectiveCounter`: while armed, the typed message's length
    /// against the objective limit.
    pub(crate) fn render_goal_counter(
        &self,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.goals.armed || !self.goals_enabled() {
            return None;
        }
        let length = session_goal::objective_length(&self.composer.read(cx).value());
        if length == 0 {
            return None;
        }
        let label = locale::strings(cx).get("chat.goal.counter.aria");
        Some(
            div()
                .id("composer-goal-counter")
                .accessibility_id("composer-goal-counter")
                .aria_label(label)
                .flex_shrink_0()
                .css_text(0.6875, 1.0, cx)
                .text_color(if length > OBJECTIVE_CHAR_LIMIT {
                    palette.status_error
                } else {
                    palette.muted_foreground
                })
                .child(format!("{length}/{OBJECTIVE_CHAR_LIMIT}"))
                .test_support()
                .into_any_element(),
        )
    }

    /// The goal as the composer's top row: status, objective, usage and an
    /// inline Pause or Resume. The dialog opens from the target button.
    pub(crate) fn render_goal_row(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.goals_enabled() {
            return None;
        }
        let session = self.selected.as_ref()?;
        let goal = self.selected_goal()?;
        let strings = locale::strings(cx);
        let key = self.mutation_key(session);
        let busy = self.goals.busy.contains(&key);
        let objective = self.goal_objective(session, goal);
        // The agent stopped but the goal is still active: the server sits out
        // its quiet window and runs the progress check.
        let evaluating = goal.status == session_goal::GoalStatus::Active
            && !matches!(
                self.live_status.get(session),
                Some(LiveStatus::Busy | LiveStatus::Retry)
            );
        let meta = |text: SharedString| {
            div()
                .flex_shrink_0()
                .css_text(0.75, 1.0, cx)
                .text_color(palette.muted_foreground)
                .child(text)
        };
        let status = if evaluating {
            div()
                .flex_shrink_0()
                .flex_row_from(direction)
                .items_center()
                .gap(px(4.0))
                .child(glyph("loader-4", 12.0, palette.muted_foreground))
                .child(meta(strings.get("chat.goal.status.evaluating")))
                .into_any_element()
        } else {
            meta(strings.get(goal.status.label_key())).into_any_element()
        };
        let action = goal.row_action().map(|action| {
            let label = strings.get(action.label_key());
            let muted = palette.muted_foreground;
            let foreground = palette.foreground;
            let button = div()
                .id("goal-row-action")
                .accessibility_id("goal-row-action")
                .role(Role::Button)
                .aria_label(label.clone())
                .h(px(28.0))
                .px(px(8.0))
                .flex_shrink_0()
                .flex_row_from(direction)
                .items_center()
                .gap(px(4.0))
                .rounded(px(6.0))
                .text_color(muted)
                .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                .child(glyph(action.icon(), 12.0, muted))
                .child(div().css_text(0.875, 1.25, cx).child(label));
            if busy {
                button
                    .opacity(0.5)
                    .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
                    .test_support()
                    .into_any_element()
            } else {
                let change = match action {
                    session_goal::GoalRowAction::Pause => GoalStatusChange::Paused,
                    session_goal::GoalRowAction::Resume => GoalStatusChange::Active,
                };
                on_activate(
                    button
                        .cursor_pointer()
                        .hover(move |style| style.text_color(foreground)),
                    cx,
                    move |this, window, cx| this.change_goal_status(change, window, cx),
                )
                .test_support()
                .into_any_element()
            }
        });
        Some(
            div()
                .id("goal-row")
                .accessibility_id("goal-row")
                .role(Role::Group)
                .aria_label(strings.get("chat.goal.row.aria"))
                .w_full()
                .min_h(px(ROW_HEIGHT))
                .min_w_0()
                .padding_start(direction, px(12.0))
                .padding_end(direction, px(6.0))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .border_b_1()
                .border_color(palette.border.opacity(0.6))
                .child(glyph(
                    "target",
                    14.0,
                    tone_color(goal.status.tone(), palette),
                ))
                .child(
                    div()
                        .id("goal-row-objective")
                        .accessibility_id("goal-row-objective")
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .css_text(0.875, 1.25, cx)
                        .text_color(palette.foreground)
                        .child(
                            objective
                                .filter(|text| !text.is_empty())
                                .map(SharedString::from)
                                .unwrap_or_else(|| strings.get("chat.goal.dialog.titleManage")),
                        )
                        .test_support(),
                )
                .child(status)
                .children(
                    goal.row_usage()
                        .map(|usage| meta(usage_text(usage, strings))),
                )
                .children(action)
                .test_support()
                .into_any_element(),
        )
    }

    /// Opens the window dialog that renders [`Self::goals`]' dialog state.
    pub(crate) fn show_goal_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = workspace.upgrade() else {
                return dialog;
            };
            let palette = cx.global::<Palette>().clone();
            let strings = locale::strings(cx).clone();
            let direction = strings.direction();
            let state = this.read(cx);
            let Some(open) = state.goals.dialog.as_ref() else {
                return dialog;
            };
            let goal = state
                .sessions
                .get(&open.key.session)
                .and_then(|record| record.goal.clone());
            let completed = goal.as_ref().is_some_and(|goal| !goal.is_live());
            let objective_text = goal
                .as_ref()
                .and_then(|goal| state.goal_objective(&open.key.session, goal));
            let can_save = state.goal_dialog_can_save(cx);
            let cannot_check = goal.is_none() && !state.goal_check_available();
            let busy = open.busy;
            let budget_enabled = open.budget_enabled;
            let objective_field = open.objective.clone();
            let budget_field = open.budget.clone();
            let length =
                session_goal::objective_length(&objective_field.read(cx).value()).to_string();
            let title = strings.get(if goal.is_some() {
                "chat.goal.dialog.titleManage"
            } else {
                "chat.goal.dialog.titleCreate"
            });
            let text_button = |id: &'static str,
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
            let on_close = workspace.clone();
            let on_cancel = workspace.clone();
            let on_save = workspace.clone();
            let on_clear = workspace.clone();
            let on_budget = workspace.clone();
            let mut body = div().flex().flex_col().gap(px(12.0));
            if let Some(goal) = &goal {
                body = body.child(
                    div()
                        .id("goal-dialog-status")
                        .accessibility_id("goal-dialog-status")
                        .p(px(8.0))
                        .rounded(px(8.0))
                        .bg(palette.elevated)
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(
                            div()
                                .flex_row_from(direction)
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .size(px(8.0))
                                        .rounded_full()
                                        .flex_shrink_0()
                                        .bg(tone_color(goal.status.tone(), &palette)),
                                )
                                .child(
                                    div()
                                        .text_size(gpui_kit::rems(0.8125))
                                        .text_color(palette.foreground)
                                        .child(strings.get(goal.status.label_key())),
                                )
                                .child(
                                    div()
                                        .text_size(gpui_kit::rems(0.75))
                                        .text_color(palette.muted_foreground)
                                        .child(format!(
                                            "{} · {}",
                                            usage_text(goal.dialog_usage(), &strings),
                                            strings.format(
                                                "chat.goal.usage.turns",
                                                &[("turns", &goal.turns_used.to_string())],
                                            )
                                        )),
                                ),
                        )
                        .when(
                            goal.status.shows_reason() && !goal.status_reason.is_empty(),
                            |this| {
                                this.child(
                                    div()
                                        .text_size(gpui_kit::rems(0.75))
                                        .text_color(palette.muted_foreground)
                                        .child(goal.status_reason.clone()),
                                )
                            },
                        )
                        .when_some(goal.evaluation_model(), |this, model| {
                            this.child(
                                div()
                                    .flex_row_from(direction)
                                    .gap(px(8.0))
                                    .text_size(gpui_kit::rems(0.75))
                                    .child(div().text_color(palette.muted_foreground).child(
                                        strings.get("chat.goal.dialog.evaluationModelLabel"),
                                    ))
                                    .child(
                                        div().min_w_0().text_color(palette.foreground).child(model),
                                    ),
                            )
                        })
                        .test_support(),
                );
            }
            if completed {
                body = body.child(
                    div()
                        .id("goal-dialog-objective-text")
                        .accessibility_id("goal-dialog-objective-text")
                        .max_h(px(192.0))
                        .overflow_y_scroll()
                        .text_size(gpui_kit::rems(0.75))
                        .text_color(palette.muted_foreground)
                        .child(
                            objective_text
                                .or_else(|| goal.as_ref().map(|goal| goal.objective.clone()))
                                .unwrap_or_default(),
                        )
                        .test_support(),
                );
            } else {
                body = body
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .flex_row_from(direction)
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(gpui_kit::rems(0.8125))
                                            .text_color(palette.foreground)
                                            .child(strings.get("chat.goal.dialog.objectiveLabel")),
                                    )
                                    .child(
                                        div()
                                            .text_size(gpui_kit::rems(0.6875))
                                            .text_color(palette.muted_foreground)
                                            .child(format!("{length}/{OBJECTIVE_CHAR_LIMIT}")),
                                    ),
                            )
                            .child(
                                Textarea::new(&objective_field)
                                    .aria_label(strings.get("chat.goal.dialog.objectiveLabel")),
                            ),
                    )
                    .child(
                        div()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(32.0))
                            .child(
                                div()
                                    .id("goal-dialog-budget")
                                    .role(Role::CheckBox)
                                    .accessibility_id("goal-dialog-budget")
                                    .aria_label(strings.get("chat.goal.dialog.budgetLabel"))
                                    .aria_toggled(if budget_enabled {
                                        gpui_kit::Toggled::True
                                    } else {
                                        gpui_kit::Toggled::False
                                    })
                                    .flex_row_from(direction)
                                    .items_center()
                                    .gap(px(8.0))
                                    .cursor_pointer()
                                    .child(glyph(
                                        if budget_enabled {
                                            "checkbox"
                                        } else {
                                            "checkbox-blank"
                                        },
                                        16.0,
                                        if budget_enabled {
                                            palette.primary
                                        } else {
                                            palette.muted_foreground
                                        },
                                    ))
                                    .child(
                                        div()
                                            .text_size(gpui_kit::rems(0.8125))
                                            .text_color(palette.foreground)
                                            .child(strings.get("chat.goal.dialog.budgetLabel")),
                                    )
                                    .on_click(move |_, _, cx| {
                                        let _ = on_budget.update(cx, |this, cx| {
                                            this.toggle_goal_dialog_budget(cx)
                                        });
                                    })
                                    .test_support(),
                            )
                            .when(budget_enabled, |this| {
                                this.child(
                                    div().w(px(140.0)).child(
                                        Input::new(&budget_field).aria_label(
                                            strings.get("chat.goal.dialog.budgetLabel"),
                                        ),
                                    ),
                                )
                            }),
                    );
            }
            if cannot_check {
                body = body.child(
                    div()
                        .text_size(gpui_kit::rems(0.75))
                        .text_color(palette.muted_foreground)
                        .child(strings.get("chat.goal.dialog.noSmallModel")),
                );
            }
            let footer = div()
                .w_full()
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .when(goal.is_some(), |this| {
                    this.child(
                        text_button(
                            "goal-dialog-clear",
                            strings.get("chat.goal.action.clear"),
                            Some(palette.status_error),
                            palette.status_error_foreground,
                            !busy,
                        )
                        .when(!busy, |button| {
                            button.on_click(move |_, window, cx| {
                                let _ = on_clear
                                    .update(cx, |this, cx| this.clear_goal_dialog(window, cx));
                            })
                        }),
                    )
                })
                .child(div().flex_1())
                .child(
                    text_button(
                        "goal-dialog-cancel",
                        strings.get("chat.goal.action.cancel"),
                        None,
                        palette.foreground,
                        !busy,
                    )
                    .when(!busy, |button| {
                        button.on_click(move |_, window, cx| {
                            let _ =
                                on_cancel.update(cx, |this, cx| this.close_goal_dialog(window, cx));
                        })
                    }),
                )
                .when(!completed, |this| {
                    this.child(
                        text_button(
                            "goal-dialog-save",
                            strings.get(if goal.is_some() {
                                "chat.goal.action.save"
                            } else {
                                "chat.goal.action.start"
                            }),
                            Some(palette.primary),
                            palette.primary_foreground,
                            can_save,
                        )
                        .when(can_save, |button| {
                            button.on_click(move |_, window, cx| {
                                let _ = on_save
                                    .update(cx, |this, cx| this.save_goal_dialog(window, cx));
                            })
                        }),
                    )
                });
            dialog
                .w(px(512.0))
                .on_close(move |_, _, cx| {
                    let _ = on_close.update(cx, |this, cx| {
                        this.goals.dialog = None;
                        cx.notify();
                    });
                })
                .title(title)
                .child(body)
                .footer(footer)
        });
        cx.notify();
    }
}
