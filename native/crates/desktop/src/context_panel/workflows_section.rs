//! The work status card's Workflows section
//! (`WorkStatusWorkflowsSection.tsx`): the workflow runs the selected session
//! proposed, newest first, each with its state and the one action that fits
//! it.
//!
//! The web has two layouts and picks by whether the Workflows panel exists.
//! This client has no Workflows panel, so it draws the layout the web uses
//! without one: a run unfolds into its phases and agents here, an open run
//! unfolded until the user folds it, and an agent's row opens the agent's
//! session. A run's agents are left out of the Subagents section
//! (`Workspace::workflow_agent_sessions`), so each session shows in one
//! place.

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AnyElement, Context, Hsla, InteractiveElement, IntoElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::workflow_runs::{
    PhaseGroup, WorkflowAgent, WorkflowAgentState, WorkflowRun, WorkflowRunAction, WorkflowRunState,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, glyph};
use crate::context_panel::{
    ActionTone, ROW_HEIGHT, ROW_LINE, ROW_SIZE, Section, collapsible_section, row_action,
    section_expanded, toggle_section,
};
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The section's key in the stored fold states (`workStatusExpandedSections`).
const SECTION_ID: &str = "workflows";

/// The list scrolls past this height (`max-h-72`).
const LIST_MAX_HEIGHT: f32 = 288.0;
/// The 14 px icon slot every row's state glyph is centred in, and the glyph.
const GLYPH_SLOT: f32 = 14.0;
const GLYPH_SIZE: f32 = 12.0;
/// Where a phase row (`ps-5`) and an agent row (`ps-8`) start.
const PHASE_INSET: f32 = 20.0;
const AGENT_INSET: f32 = 32.0;

/// How a state reads: its glyph, the glyph's colour and its name
/// (`workflowRunPresentation.ts`). Without a colour the glyph is muted.
struct Look {
    icon: &'static str,
    color: Option<Hsla>,
    label_key: &'static str,
}

fn run_look(state: WorkflowRunState, palette: &Palette) -> Look {
    let (icon, color, label_key) = match state {
        WorkflowRunState::AwaitingApproval => (
            "time",
            Some(palette.status_warning),
            "chat.workflow.state.awaitingApproval",
        ),
        WorkflowRunState::Running => (
            "record-circle",
            Some(palette.status_info),
            "chat.workflow.state.running",
        ),
        WorkflowRunState::Completed => (
            "checkbox-circle",
            Some(palette.status_success),
            "chat.workflow.state.completed",
        ),
        WorkflowRunState::Failed => (
            "close-circle",
            Some(palette.status_error),
            "chat.workflow.state.failed",
        ),
        WorkflowRunState::Stopped => ("stop", None, "chat.workflow.state.stopped"),
        WorkflowRunState::Declined => ("close-circle", None, "chat.workflow.state.declined"),
    };
    Look {
        icon,
        color,
        label_key,
    }
}

fn agent_look(state: WorkflowAgentState, palette: &Palette) -> Look {
    let (icon, color, label_key) = match state {
        WorkflowAgentState::Queued => ("time", None, "chat.workflow.agent.queued"),
        WorkflowAgentState::Running => (
            "record-circle",
            Some(palette.status_info),
            "chat.workflow.agent.running",
        ),
        WorkflowAgentState::Completed => (
            "checkbox-circle",
            Some(palette.status_success),
            "chat.workflow.agent.completed",
        ),
        WorkflowAgentState::Failed => (
            "close-circle",
            Some(palette.status_error),
            "chat.workflow.agent.failed",
        ),
        WorkflowAgentState::Stopped => ("stop", None, "chat.workflow.agent.stopped"),
    };
    Look {
        icon,
        color,
        label_key,
    }
}

/// The actions a run's row offers, in order, with each one's tone
/// (`runAction`). A finished or declined run offers none: its glyph says how
/// it ended.
pub(crate) fn run_actions(state: WorkflowRunState) -> &'static [(WorkflowRunAction, ActionTone)] {
    match state {
        WorkflowRunState::AwaitingApproval => &[
            (WorkflowRunAction::Approve, ActionTone::Info),
            (WorkflowRunAction::Decline, ActionTone::Default),
        ],
        WorkflowRunState::Running => &[(WorkflowRunAction::Stop, ActionTone::Default)],
        WorkflowRunState::Failed | WorkflowRunState::Stopped => {
            &[(WorkflowRunAction::Resume, ActionTone::Default)]
        }
        WorkflowRunState::Completed | WorkflowRunState::Declined => &[],
    }
}

fn action_label_key(action: WorkflowRunAction) -> &'static str {
    match action {
        WorkflowRunAction::Approve => "chat.workflow.action.run",
        WorkflowRunAction::Decline => "chat.workflow.action.decline",
        WorkflowRunAction::Stop => "chat.workflow.action.stop",
        WorkflowRunAction::Resume => "chat.workflow.action.resume",
    }
}

/// The section's heading aggregate: open runs over all runs, or the count
/// alone once none is open.
pub(crate) fn summary(runs: &[WorkflowRun]) -> String {
    let open = runs.iter().filter(|run| run.is_open()).count();
    if open > 0 {
        format!("{open}/{}", runs.len())
    } else {
        runs.len().to_string()
    }
}

/// What a row's tooltip says: the error when there is one, else `fallback`;
/// nothing when that text is empty.
fn tooltip_text(error: Option<&str>, fallback: Option<&str>) -> Option<SharedString> {
    error
        .or(fallback)
        .filter(|text| !text.is_empty())
        .map(|text| SharedString::from(text.to_owned()))
}

/// The parts of one 28 px row (`WorkStatusRow`).
struct Row {
    id: SharedString,
    /// Where the row's content starts.
    inset: f32,
    leading: Option<Look>,
    label: SharedString,
    aria_label: SharedString,
    tooltip: Option<SharedString>,
    /// What sits at the row's end, `gap-1.5` apart.
    trailing: Vec<AnyElement>,
}

impl Row {
    /// The row's frame and content; the caller adds what a press does.
    fn frame(
        self,
        role: Role,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Workspace>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let muted = palette.muted_foreground;
        let trailing = (!self.trailing.is_empty()).then(|| {
            div()
                .flex_shrink_0()
                .flex_row_from(direction)
                .items_center()
                .gap(px(6.0))
                .children(self.trailing)
        });
        div()
            .id(self.id.clone())
            .role(role)
            .accessibility_id(self.id)
            .aria_label(self.aria_label)
            .h(px(ROW_HEIGHT))
            .padding_start(direction, px(self.inset))
            .padding_end(direction, px(4.0))
            .rounded(px(CONTROL_RADIUS))
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .text_color(muted)
            .children(self.leading.map(|look| {
                div()
                    .size(px(GLYPH_SLOT))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(glyph(look.icon, GLYPH_SIZE, look.color.unwrap_or(muted)))
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .css_text(ROW_SIZE, ROW_LINE, cx)
                    .child(self.label),
            )
            .children(trailing)
            .when_some(self.tooltip, |row, text| {
                row.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
            })
    }
}

impl Workspace {
    /// The section, for a selected session that proposed at least one run
    /// while workflows are shown.
    pub(crate) fn render_workflows_section(
        &self,
        palette: &Palette,
        direction: Direction,
        first_heading: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.selected.as_ref()?;
        let runs = self.session_workflow_runs(session);
        if runs.is_empty() {
            return None;
        }
        let strings = locale::strings(cx);
        let mut rows = Vec::new();
        for run in runs.iter().rev() {
            rows.push(self.workflow_run_row(run, strings, palette, direction, cx));
            // Open runs show their phases; a finished one folds to its row
            // until asked.
            if run.is_open() == self.workflow_runs.is_toggled(&run.id) {
                continue;
            }
            for phase in run.phase_groups() {
                rows.push(phase_row(run, &phase, strings, palette, direction, cx));
                for agent in &phase.agents {
                    rows.push(agent_row(run, agent, strings, palette, direction, cx));
                }
            }
        }
        let list = div()
            .id("work-status-workflows-list")
            .max_h(px(LIST_MAX_HEIGHT))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .children(rows)
            .into_any_element();
        Some(collapsible_section(
            Section {
                id: "work-status-workflows",
                icon: "flow-chart",
                title: strings.get("chat.workStatus.section.workflows"),
                summary: Some(summary(runs).into()),
                expanded: section_expanded(SECTION_ID, true, cx),
                toggle: |_, cx| toggle_section(SECTION_ID, true, cx),
            },
            vec![list],
            palette,
            direction,
            first_heading,
            cx,
        ))
    }

    /// A run's row: a press folds or unfolds its phases, and its trailing
    /// pills send the action that fits its state.
    fn workflow_run_row(
        &self,
        run: &WorkflowRun,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let look = run_look(run.state, palette);
        let busy = self.workflow_runs.is_pending(&run.id);
        let trailing = run_actions(run.state)
            .iter()
            .map(|(action, tone)| {
                let (action, id) = (*action, run.id.clone());
                row_action(
                    format!("work-status-workflow-{}-{}", action.as_str(), run.id),
                    strings.get(action_label_key(action)),
                    *tone,
                    busy,
                    palette,
                    cx,
                    move |this, window, cx| this.act_on_workflow_run(&id, action, window, cx),
                )
            })
            .collect();
        let foreground = palette.foreground;
        let toggled = run.id.clone();
        let row = Row {
            id: format!("work-status-workflow-{}", run.id).into(),
            inset: 4.0,
            aria_label: format!("{}. {}", run.name, strings.get(look.label_key)).into(),
            leading: Some(look),
            label: run.name.clone().into(),
            tooltip: tooltip_text(run.error.as_deref(), Some(run.description.as_str())),
            trailing,
        };
        on_activate(
            row.frame(Role::Button, palette, direction, cx)
                .cursor_pointer()
                .hover(move |style| style.text_color(foreground)),
            cx,
            move |this, _, cx| {
                this.workflow_runs.toggle_run(&toggled);
                cx.notify();
            },
        )
        .test_support()
        .into_any_element()
    }
}

/// A phase's row: its title, or the name agents outside a phase go under,
/// and how many of its agents are done.
fn phase_row(
    run: &WorkflowRun,
    phase: &PhaseGroup<'_>,
    strings: &Strings,
    palette: &Palette,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    let label: SharedString = match phase.title {
        Some(title) => title.to_owned().into(),
        None => strings.get("chat.workflow.phase.unphased"),
    };
    let trailing = if phase.agents.is_empty() {
        Vec::new()
    } else {
        vec![
            div()
                .css_text(ROW_SIZE, ROW_LINE, cx)
                .child(format!("{}/{}", phase.done, phase.agents.len()))
                .into_any_element(),
        ]
    };
    Row {
        id: format!(
            "work-status-workflow-{}-phase-{}",
            run.id,
            phase.title.unwrap_or_default()
        )
        .into(),
        inset: PHASE_INSET,
        leading: None,
        aria_label: label.clone(),
        label,
        tooltip: None,
        trailing,
    }
    .frame(Role::Group, palette, direction, cx)
    .test_support()
    .into_any_element()
}

/// An agent's row. A press opens the agent's session once the server made
/// one; a failed agent says so and a reused answer is marked.
fn agent_row(
    run: &WorkflowRun,
    agent: &WorkflowAgent,
    strings: &Strings,
    palette: &Palette,
    direction: Direction,
    cx: &Context<Workspace>,
) -> AnyElement {
    let look = agent_look(agent.state, palette);
    let state = strings.get(look.label_key);
    let failed = agent.state == WorkflowAgentState::Failed;
    let trailing = if failed || agent.replayed {
        vec![
            div()
                .css_text(ROW_SIZE, ROW_LINE, cx)
                .when(failed, |value| value.text_color(palette.status_error))
                .child(if failed {
                    state.clone()
                } else {
                    strings.get("chat.workflow.agent.reused")
                })
                .into_any_element(),
        ]
    } else {
        Vec::new()
    };
    let open = strings.format(
        "chat.workStatus.workflow.openAgent",
        &[("name", &agent.label)],
    );
    let row = Row {
        id: format!("work-status-workflow-{}-agent-{}", run.id, agent.key).into(),
        inset: AGENT_INSET,
        leading: Some(look),
        label: agent.label.clone().into(),
        aria_label: format!("{open}. {state}").into(),
        tooltip: tooltip_text(agent.error.as_deref(), agent.preview.as_deref()),
        trailing,
    };
    let Some(session) = agent.session_id.clone() else {
        return row
            .frame(Role::Group, palette, direction, cx)
            .test_support()
            .into_any_element();
    };
    let foreground = palette.foreground;
    on_activate(
        row.frame(Role::Button, palette, direction, cx)
            .cursor_pointer()
            .hover(move |style| style.text_color(foreground)),
        cx,
        move |this, window, cx| this.open_workflow_agent(session.clone(), window, cx),
    )
    .test_support()
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, ElementId, TestAppContext, point};
    use openchamber_native_core::ids::SessionId;
    use openchamber_native_core::workflow_runs::WorkflowRunId;
    use serde_json::json;

    use crate::settings_test_server::{Server, settle};
    use crate::workflow_runs::tests::{attach, connect, summary as run_summary};

    use super::*;

    fn run(state: WorkflowRunState) -> WorkflowRun {
        WorkflowRun {
            id: WorkflowRunId::from("run"),
            parent_session_id: SessionId::from("ses_parent"),
            directory: "/repo".to_owned(),
            name: "Audit".to_owned(),
            description: String::new(),
            state,
            created_at: 1,
            current_phase: None,
            phases: Vec::new(),
            agents: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn each_state_offers_the_one_action_that_fits_it() {
        let actions = |state| -> Vec<WorkflowRunAction> {
            run_actions(state)
                .iter()
                .map(|(action, _)| *action)
                .collect()
        };
        assert_eq!(
            actions(WorkflowRunState::AwaitingApproval),
            [WorkflowRunAction::Approve, WorkflowRunAction::Decline]
        );
        assert_eq!(
            run_actions(WorkflowRunState::AwaitingApproval)[0].1,
            ActionTone::Info,
            "running the workflow is the toned choice"
        );
        assert_eq!(
            actions(WorkflowRunState::Running),
            [WorkflowRunAction::Stop]
        );
        assert_eq!(
            actions(WorkflowRunState::Failed),
            [WorkflowRunAction::Resume]
        );
        assert_eq!(
            actions(WorkflowRunState::Stopped),
            [WorkflowRunAction::Resume]
        );
        assert!(actions(WorkflowRunState::Completed).is_empty());
        assert!(actions(WorkflowRunState::Declined).is_empty());
    }

    #[test]
    fn the_heading_counts_open_runs_over_all_runs() {
        assert_eq!(
            summary(&[
                run(WorkflowRunState::Running),
                run(WorkflowRunState::AwaitingApproval),
                run(WorkflowRunState::Completed),
            ]),
            "2/3"
        );
        assert_eq!(
            summary(&[
                run(WorkflowRunState::Completed),
                run(WorkflowRunState::Failed)
            ]),
            "2"
        );
    }

    #[test]
    fn a_tooltip_prefers_the_error_and_is_absent_without_text() {
        assert_eq!(
            tooltip_text(Some("boom"), Some("what it does")).as_deref(),
            Some("boom")
        );
        assert_eq!(
            tooltip_text(None, Some("what it does")).as_deref(),
            Some("what it does")
        );
        assert_eq!(tooltip_text(None, Some("")), None);
        assert_eq!(tooltip_text(None, None), None);
    }

    #[gpui_kit::test]
    fn the_card_lists_runs_newest_first_and_hides_without_any(cx: &mut TestAppContext) {
        let server = Server::new(|request| {
            if !request.is("GET", "/api/openchamber/workflows") {
                return (404, json!({"error": "unknown route"}));
            }
            if request.target.contains("sessionId=ses_empty") {
                return (200, json!({"runs": []}));
            }
            (
                200,
                json!({"runs": [
                    run_summary("run_a", "completed", 1),
                    run_summary("run_b", "awaiting-approval", 2),
                ]}),
            )
        });
        let (window, workspace) = connect(&server, cx);
        let (parent, empty) = (SessionId::from("ses_parent"), SessionId::from("ses_empty"));
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.load_workflow_runs(&parent, cx);
                workspace.load_workflow_runs(&empty, cx);
            });
        })
        .expect("test window");
        settle(window, cx);

        let rendered = |session: &SessionId, cx: &mut TestAppContext| {
            cx.update_window(window, |_, _, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.selected = Some(session.clone());
                    let palette = cx.global::<Palette>().clone();
                    workspace
                        .render_workflows_section(&palette, Direction::LeftToRight, false, cx)
                        .is_some()
                })
            })
            .expect("test window")
        };
        assert!(
            rendered(&parent, cx),
            "a session with runs shows the section"
        );
        assert!(!rendered(&empty, cx), "a session without runs shows none");
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            let runs = workspace.session_workflow_runs(&parent);
            assert_eq!(summary(runs), "1/2");
            let newest_first: Vec<&str> = runs.iter().rev().map(|run| run.id.as_str()).collect();
            assert_eq!(newest_first, ["run_b", "run_a"]);
        });

        // Folding belongs to the run: an open run starts unfolded, a finished
        // one folded, and a press flips only the run pressed.
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, _| {
                let (open, finished) = (WorkflowRunId::from("run_b"), WorkflowRunId::from("run_a"));
                assert!(!workspace.workflow_runs.is_toggled(&open));
                workspace.workflow_runs.toggle_run(&open);
                assert!(workspace.workflow_runs.is_toggled(&open));
                assert!(!workspace.workflow_runs.is_toggled(&finished));
                workspace.workflow_runs.toggle_run(&open);
                assert!(!workspace.workflow_runs.is_toggled(&open));
            });
        })
        .expect("test window");
    }

    #[gpui_kit::test]
    fn a_held_pill_takes_the_press_without_acting_or_folding_its_row(cx: &mut TestAppContext) {
        let server = Server::new(|request| {
            if request.method == "POST" {
                return (200, json!({"ok": true}));
            }
            if request.is("GET", "/api/openchamber/workflows") {
                return (
                    200,
                    json!({"runs": [run_summary("run_a", "awaiting-approval", 1)]}),
                );
            }
            (404, json!({"error": "unknown route"}))
        });
        // A real session is selected while the source is still synthetic, so
        // the card is drawn for it; then the server is attached.
        let (window, workspace) = crate::workspace_tests::open(cx);
        let parent = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let parent = workspace
                        .sessions
                        .active()
                        .first()
                        .map(|session| session.id.clone())
                        .expect("the synthetic source lists sessions");
                    workspace.select_session(parent.clone(), window, cx);
                    parent
                })
            })
            .expect("test window");
        attach(&server, window, &workspace, cx);
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                assert_eq!(workspace.selected.as_ref(), Some(&parent));
                workspace.load_workflow_runs(&parent, cx);
            });
        })
        .expect("test window");
        settle(window, cx);

        let run = WorkflowRunId::from("run_a");
        let press = |id: &'static str, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click(ElementId::Name(id.into()), cx);
            })
            .expect("test window");
            settle(window, cx);
        };
        let state = |cx: &mut TestAppContext| {
            let posts = server
                .requests()
                .iter()
                .filter(|request| request.method == "POST")
                .count();
            (
                posts,
                cx.read(|cx| workspace.read(cx).workflow_runs.is_toggled(&run)),
            )
        };
        let hold = |held: bool, cx: &mut TestAppContext| {
            cx.update_window(window, |_, _, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.workflow_runs.set_pending(&run, held);
                    cx.notify();
                });
            })
            .expect("test window");
        };

        // Held: the press sends nothing and does not reach the row.
        hold(true, cx);
        press("work-status-workflow-approve-run_a", cx);
        assert_eq!(state(cx), (0, false));

        // Free: the press acts, and still does not reach the row.
        hold(false, cx);
        press("work-status-workflow-approve-run_a", cx);
        assert_eq!(state(cx), (1, false));

        // The row itself folds its run. The press lands on the row's name:
        // the row's centre can lie under its pills.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click_at(
                ElementId::Name("work-status-workflow-run_a".into()),
                point(px(40.0), px(8.0)),
                cx,
            );
        })
        .expect("test window");
        settle(window, cx);
        assert_eq!(state(cx), (1, true));
    }
}
