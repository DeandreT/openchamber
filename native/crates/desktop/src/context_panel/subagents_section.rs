//! The work status card's Subagents section
//! (`WorkStatusSubagentsSection.tsx`): the selected session's child
//! sessions, what each is doing and what its subtree has cost.
//!
//! A permission request or question raised by a child has no place in the
//! parent's transcript, so this list is where it shows. The rows come from
//! the live session list, the running state, the turn observations and the
//! pending requests the workspace already holds; the section reads nothing
//! from the server and scans no history.

use std::collections::HashSet;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AnyElement, App, Context, InteractiveElement, IntoElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::pending_requests::RequestKind;
use openchamber_native_core::request_owner::RequestOwner;
use openchamber_native_core::send_config;
use openchamber_native_core::session::SessionRecord;
use openchamber_native_core::subagents::{
    self, ActivityDuration, ChildFacts, SubagentRow, SubagentState,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, glyph};
use crate::context_panel::{ROW_HEIGHT, ROW_LINE, ROW_SIZE, Section, collapsible_section};
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::transcript_clock::now_millis;
use crate::ui_memory::{self, UiMemory};
use crate::workspace::Workspace;

/// The section's key in `workStatusExpandedSections`.
const SECTION_ID: &str = "subagents";
/// `max-h-56`: the expanded list scrolls past eight rows.
const LIST_MAX_HEIGHT: f32 = 224.0;
/// The status glyph, centred in the panel's 14 px icon slot.
const STATUS_SLOT: f32 = 14.0;
const STATUS_GLYPH: f32 = 12.0;

/// The sessions with a pending permission request and with a pending
/// question, among the requests the user is shown.
struct Waiting<'a> {
    permission: HashSet<&'a SessionId>,
    question: HashSet<&'a SessionId>,
}

impl Workspace {
    fn sessions_waiting_on_user(&self) -> Waiting<'_> {
        let mut waiting = Waiting {
            permission: HashSet::new(),
            question: HashSet::new(),
        };
        for (scope, store) in &self.blocking.stores {
            if scope.runtime() != &self.runtime {
                continue;
            }
            for (key, _) in store.entries() {
                let RequestOwner::Session(owner) = &key.owner else {
                    continue;
                };
                if self.request_held(&key.owner, &key.id) {
                    continue;
                }
                match key.id.kind() {
                    RequestKind::Permission => waiting.permission.insert(owner),
                    RequestKind::Form => waiting.question.insert(owner),
                };
            }
        }
        waiting
    }

    /// The selected session's rows, in display order. Empty without a
    /// selected session and for a session without listed children.
    pub(crate) fn subagent_rows(&self) -> Vec<SubagentRow<'_>> {
        let Some(parent) = self.selected_session() else {
            return Vec::new();
        };
        let waiting = self.sessions_waiting_on_user();
        // Workflow agents carry their mark; the held runs name the agents
        // started before sessions carried one.
        let workflow_agents = self.workflow_agent_sessions(&parent.id);
        subagents::rows(
            self.sessions.active(),
            &parent.id,
            &workflow_agents,
            |child: &SessionRecord| ChildFacts {
                permission: waiting.permission.contains(&child.id),
                question: waiting.question.contains(&child.id),
                live: self.live_status.get(&child.id).copied(),
                outcome: self.session_activity.outcome(&child.id),
                snapshot_covers: self.session_activity.snapshot_covers(&child.id)
                    && subagents::host_snapshot_boundary(parent, child),
            },
        )
    }

    /// Whether the section shows its rows: it just opened itself, or the
    /// user's stored choice says so, or there is no stored choice. This is
    /// the value drawn and the value the fold toggle flips.
    pub(crate) fn subagents_expanded(&self, cx: &App) -> bool {
        self.subagents_reopened.get()
            || cx
                .try_global::<UiMemory>()
                .and_then(|memory| memory.work_status_section_expanded(SECTION_ID))
                .unwrap_or(true)
    }

    /// Folds or unfolds the section from what is on screen. A reopening
    /// that has not been stored yet is settled here, so its later write
    /// cannot undo the user's choice.
    fn toggle_subagents(&mut self, cx: &mut Context<Self>) {
        let expanded = self.subagents_expanded(cx);
        self.subagents_reopened.set(false);
        ui_memory::set_work_status_section_expanded(SECTION_ID, !expanded, cx);
    }

    /// The section, absent while the selected session has no listed child.
    pub(crate) fn render_subagents(
        &self,
        palette: &Palette,
        direction: Direction,
        first_heading: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let rows = self.subagent_rows();
        let present = !rows.is_empty();
        // Children appearing where there were none is when the section has
        // something urgent to say, so it opens itself; only on that edge, so
        // it never fights a user who just folded it.
        let before = self.subagents_present.replace(Some(present));
        let appeared = present && before == Some(false);
        if appeared && !self.subagents_expanded(cx) {
            // Drawn expanded from this frame on; the stored state follows
            // after the frame, unless the user folds the section first.
            self.subagents_reopened.set(true);
            cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |this, cx| {
                    if this.subagents_reopened.replace(false) {
                        ui_memory::set_work_status_section_expanded(SECTION_ID, true, cx);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        if !present {
            return None;
        }
        let strings = locale::strings(cx);
        let now = now_millis();
        let elements: Vec<AnyElement> = rows
            .iter()
            .map(|row| self.render_subagent_row(row, now, strings, palette, direction, cx))
            .collect();
        let list = div()
            .id("work-status-subagents-list")
            .max_h(px(LIST_MAX_HEIGHT))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .children(elements)
            .into_any_element();
        Some(collapsible_section(
            Section {
                id: "work-status-subagents",
                icon: "ai-agent",
                title: strings.get("chat.workStatus.section.subagents"),
                summary: Some(subagents::summary(&rows).into()),
                expanded: self.subagents_expanded(cx),
                toggle: Self::toggle_subagents,
            },
            vec![list],
            palette,
            direction,
            first_heading,
            cx,
        ))
    }

    /// One child: its state glyph, its title, what waits on the user or how
    /// long its turn has run, and its subtree's cost. Activating it opens
    /// the child.
    fn render_subagent_row(
        &self,
        row: &SubagentRow<'_>,
        now: i64,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let child = row.session;
        let title = child.title.trim();
        let label: SharedString = if title.is_empty() {
            strings.get("chat.workStatus.subagent.untitled")
        } else {
            SharedString::from(title.to_owned())
        };
        let (icon, color, status_key) = match row.state {
            SubagentState::NeedsPermission => (
                "alert",
                Some(palette.status_warning),
                Some("chat.workStatus.subagent.needsPermission"),
            ),
            SubagentState::AskedQuestion => (
                "alert",
                Some(palette.status_warning),
                Some("chat.workStatus.subagent.askedQuestion"),
            ),
            SubagentState::Working => (
                "record-circle",
                Some(palette.status_info),
                Some("chat.workStatus.subagent.working"),
            ),
            SubagentState::Failed => (
                "close-circle",
                Some(palette.status_error),
                Some("chat.workStatus.subagent.failed"),
            ),
            SubagentState::Done => (
                "checkbox-circle",
                Some(palette.status_success),
                Some("chat.workStatus.subagent.done"),
            ),
            SubagentState::Unknown => ("time", None, None),
        };
        let open_label =
            strings.format("chat.workStatus.action.openSubagent", &[("name", &*label)]);
        let aria_label: SharedString = match status_key {
            Some(key) => format!("{open_label}. {}", strings.get(key)).into(),
            None => open_label,
        };

        let mut values: Vec<AnyElement> = Vec::new();
        match (row.state, status_key) {
            (SubagentState::NeedsPermission | SubagentState::AskedQuestion, Some(key)) => {
                values.push(
                    div()
                        .css_text(ROW_SIZE, ROW_LINE, cx)
                        .text_color(palette.status_warning)
                        .child(strings.get(key))
                        .into_any_element(),
                );
            }
            // A running turn with no known start shows no duration.
            (SubagentState::Working, _) => {
                if let Some(started_at) = self.session_activity.started_at(&child.id) {
                    self.tick_live_indicators(cx);
                    values.push(
                        div()
                            .css_text(ROW_SIZE, ROW_LINE, cx)
                            .text_color(palette.status_info)
                            .child(duration_label(started_at, now, strings))
                            .into_any_element(),
                    );
                }
            }
            _ => {}
        }
        if let Some(cost) = row.cost {
            values.push(
                div()
                    .css_text(ROW_SIZE, ROW_LINE, cx)
                    .text_color(palette.muted_foreground)
                    .child(SharedString::from(subagents::format_cost(cost)))
                    .into_any_element(),
            );
        }

        let tooltip = self.subagent_model_name(child);
        let row_id = SharedString::from(format!("work-status-subagent-{}", child.id.as_str()));
        let group = row_id.clone();
        let foreground = palette.foreground;
        let status_glyph = match color {
            Some(color) => glyph(icon, STATUS_GLYPH, color),
            None => glyph(icon, STATUS_GLYPH, palette.muted_foreground)
                .group_hover(group.clone(), move |style| style.text_color(foreground)),
        };
        let opened = child.id.clone();
        on_activate(
            div()
                .id(row_id.clone())
                .group(group)
                .role(Role::Button)
                .accessibility_id(row_id)
                .aria_label(aria_label)
                .tab_index(0)
                .h(px(ROW_HEIGHT))
                .flex_shrink_0()
                .px(px(4.0))
                .rounded(px(CONTROL_RADIUS))
                .flex_row_from(direction)
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .text_color(palette.muted_foreground)
                .hover(move |style| style.text_color(foreground))
                .when_some(tooltip, |this, tooltip| {
                    this.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                })
                .child(
                    div()
                        .size(px(STATUS_SLOT))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(status_glyph),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .css_text(ROW_SIZE, ROW_LINE, cx)
                        .child(label),
                )
                .when(!values.is_empty(), |this| {
                    this.child(
                        div()
                            .flex_shrink_0()
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(6.0))
                            .children(values),
                    )
                }),
            cx,
            move |this, window, cx| this.open_subtask(opened.clone(), window, cx),
        )
        .test_support()
        .into_any_element()
    }

    /// The child's model as the catalog names it, for the row's tooltip.
    /// Absent while the child reports no model.
    fn subagent_model_name(&self, child: &SessionRecord) -> Option<SharedString> {
        let model = child.model.as_ref()?;
        let directory = self.config_directory()?;
        let providers = self
            .send_catalog
            .directory(&directory)
            .and_then(|entry| entry.providers.ready())
            .map(Vec::as_slice)
            .unwrap_or_default();
        Some(send_config::model_label(providers, model))
            .filter(|name| !name.is_empty())
            .map(SharedString::from)
    }
}

/// `formatSessionActivityDuration`: `7s`, `1m 23s`, `1h 2m`.
fn duration_label(started_at: i64, now: i64, strings: &Strings) -> SharedString {
    match ActivityDuration::since(started_at, now) {
        ActivityDuration::Seconds(seconds) => strings.format(
            "common.duration.secondsCompact",
            &[("seconds", &seconds.to_string())],
        ),
        ActivityDuration::MinutesSeconds { minutes, seconds } => strings.format(
            "common.duration.minutesSecondsCompact",
            &[
                ("minutes", &minutes.to_string()),
                ("seconds", &seconds.to_string()),
            ],
        ),
        ActivityDuration::HoursMinutes { hours, minutes } => strings.format(
            "common.duration.hoursMinutesCompact",
            &[
                ("hours", &hours.to_string()),
                ("minutes", &minutes.to_string()),
            ],
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext, ElementId, Entity, TestAppContext, Window};
    use openchamber_native_core::form::FormRequest;
    use openchamber_native_core::ids::RuntimeId;
    use openchamber_native_core::pending_requests::PendingRequest;
    use openchamber_native_core::session::{LiveStatus, SessionKind, SessionPatch, SessionTime};
    use openchamber_native_core::transcript::Usd;
    use openchamber_native_transport::event::{BlockingRequestEvent, ExecutionEdge, ServerEvent};

    use super::*;
    use crate::workspace_tests::open;

    fn update<T>(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> T,
    ) -> T {
        cx.update_window(window, |_, window, cx| {
            let result = workspace.update(cx, |workspace, cx| f(workspace, window, cx));
            window.render_frame(cx);
            result
        })
        .expect("window is open")
    }

    fn shown(window: AnyWindowHandle, id: &str, cx: &mut TestAppContext) -> bool {
        let id = ElementId::Name(SharedString::from(id.to_owned()));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .expect("window is open")
    }

    fn click(window: AnyWindowHandle, id: &str, cx: &mut TestAppContext) {
        let id = ElementId::Name(SharedString::from(id.to_owned()));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .expect("window is open");
    }

    fn child(id: &str, parent: &SessionRecord, created: i64) -> SessionRecord {
        SessionRecord {
            parent_id: Some(parent.id.clone()),
            title: format!("  {id} title "),
            time: SessionTime {
                created,
                updated: created,
                idle: None,
                archived: None,
            },
            ..SessionRecord::new(id, parent.directory.clone().expect("a directory"))
        }
    }

    fn created(records: Vec<SessionRecord>) -> Vec<ServerEvent> {
        records
            .into_iter()
            .map(ServerEvent::SessionCreated)
            .collect()
    }

    fn deleted(ids: &[&str]) -> Vec<ServerEvent> {
        ids.iter()
            .map(|id| ServerEvent::SessionDeleted {
                session_id: SessionId::from(*id),
            })
            .collect()
    }

    fn status(id: &str, status: LiveStatus) -> ServerEvent {
        ServerEvent::SessionStatus {
            session_id: id.into(),
            status,
        }
    }

    fn edge(id: &str, edge: ExecutionEdge) -> ServerEvent {
        ServerEvent::SessionExecution {
            session_id: id.into(),
            edge,
        }
    }

    /// Selects a childless root session of an admitted host directory and
    /// returns its record.
    fn select_parent(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) -> SessionRecord {
        update(window, workspace, cx, |this, window, cx| {
            let sessions = this.sessions.active();
            let parent = sessions
                .iter()
                .find(|record| {
                    record.parent_id.is_none()
                        && record.directory.is_some()
                        && this.session_admitted(&record.id)
                        && !sessions
                            .iter()
                            .any(|other| other.parent_id.as_ref() == Some(&record.id))
                })
                .expect("a childless root session")
                .clone();
            this.select_session(parent.id.clone(), window, cx);
            assert_eq!(this.selected.as_ref(), Some(&parent.id));
            parent
        })
    }

    fn states(this: &Workspace) -> Vec<(String, SubagentState)> {
        this.subagent_rows()
            .iter()
            .map(|row| (row.session.id.as_str().to_owned(), row.state))
            .collect()
    }

    fn state_of(this: &Workspace, id: &str) -> SubagentState {
        this.subagent_rows()
            .iter()
            .find(|row| row.session.id.as_str() == id)
            .unwrap_or_else(|| panic!("{id} is listed"))
            .state
    }

    fn expect_states(this: &Workspace, expected: &[(&str, SubagentState)]) {
        let expected: Vec<(String, SubagentState)> = expected
            .iter()
            .map(|(id, state)| ((*id).to_owned(), *state))
            .collect();
        assert_eq!(states(this), expected);
    }

    #[gpui_kit::test]
    fn the_section_is_absent_without_children_and_lists_them_when_they_appear(
        cx: &mut TestAppContext,
    ) {
        let (window, workspace) = open(cx);
        let parent = select_parent(window, &workspace, cx);
        assert!(!shown(window, "work-status-subagents", cx));
        update(window, &workspace, cx, |this, window, cx| {
            let mut agent = child("ses_agent", &parent, 30);
            agent.kind = SessionKind::WorkflowAgent;
            this.apply_server_events(
                created(vec![
                    child("ses_one", &parent, 10),
                    child("ses_two", &parent, 20),
                    agent,
                ]),
                window,
                cx,
            );
            // A workflow agent is listed by its run, and nothing says yet
            // whether the others are idle.
            expect_states(
                this,
                &[
                    ("ses_two", SubagentState::Unknown),
                    ("ses_one", SubagentState::Unknown),
                ],
            );
        });
        assert!(shown(window, "work-status-subagents", cx));
        assert!(shown(window, "work-status-subagent-ses_one", cx));
        assert!(!shown(window, "work-status-subagent-ses_agent", cx));
    }

    #[gpui_kit::test]
    fn live_facts_decide_each_row_and_one_child_does_not_change_another(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let parent = select_parent(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            let mut events = created(vec![
                child("ses_done", &parent, 10),
                child("ses_failed", &parent, 20),
                child("ses_working", &parent, 30),
                child("ses_unknown", &parent, 40),
                child("ses_asked", &parent, 50),
            ]);
            events.extend([
                edge("ses_working", ExecutionEdge::Started),
                status("ses_working", LiveStatus::Busy),
                status("ses_done", LiveStatus::Busy),
                status("ses_done", LiveStatus::Idle),
                edge("ses_failed", ExecutionEdge::Failed),
                status("ses_failed", LiveStatus::Idle),
                status("ses_asked", LiveStatus::Busy),
                ServerEvent::BlockingRequest {
                    directory: None,
                    event: BlockingRequestEvent::Asked(PendingRequest::Form(FormRequest {
                        id: "frm_child".into(),
                        owner: RequestOwner::Session("ses_asked".into()),
                        title: "A question".into(),
                        metadata_display: None,
                        fields: Vec::new(),
                    })),
                },
            ]);
            this.apply_server_events(events, window, cx);
            expect_states(
                this,
                &[
                    ("ses_asked", SubagentState::AskedQuestion),
                    ("ses_working", SubagentState::Working),
                    ("ses_unknown", SubagentState::Unknown),
                    ("ses_failed", SubagentState::Failed),
                    ("ses_done", SubagentState::Done),
                ],
            );
            assert_eq!(subagents::summary(&this.subagent_rows()), "2/5");
            assert!(
                this.session_activity
                    .started_at(&"ses_working".into())
                    .is_some()
            );
            assert_eq!(
                this.session_activity.started_at(&"ses_asked".into()),
                None,
                "a busy report is not a turn start"
            );

            // The failed child runs again and ends cleanly; the others stay.
            this.apply_server_events(
                vec![
                    status("ses_failed", LiveStatus::Busy),
                    status("ses_failed", LiveStatus::Idle),
                ],
                window,
                cx,
            );
            assert_eq!(state_of(this, "ses_failed"), SubagentState::Done);
            assert_eq!(state_of(this, "ses_working"), SubagentState::Working);
            assert_eq!(state_of(this, "ses_unknown"), SubagentState::Unknown);

            // Deleting one child removes its row and its observations only.
            this.apply_server_events(deleted(&["ses_working"]), window, cx);
            assert_eq!(
                this.session_activity.started_at(&"ses_working".into()),
                None
            );
            expect_states(
                this,
                &[
                    ("ses_asked", SubagentState::AskedQuestion),
                    ("ses_unknown", SubagentState::Unknown),
                    ("ses_failed", SubagentState::Done),
                    ("ses_done", SubagentState::Done),
                ],
            );
        });
        assert!(shown(window, "work-status-subagent-ses_asked", cx));
    }

    #[gpui_kit::test]
    fn absence_of_busy_is_done_only_under_a_covering_host_snapshot(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let parent = select_parent(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            let mut in_space = child("ses_space", &parent, 30);
            in_space.directory = Some("/spaces/0123456789ab/repo".to_owned());
            this.apply_server_events(
                created(vec![
                    child("ses_idle", &parent, 10),
                    child("ses_running", &parent, 20),
                    in_space,
                ]),
                window,
                cx,
            );
            assert_eq!(state_of(this, "ses_idle"), SubagentState::Unknown);

            this.apply_status_snapshot(
                HashMap::from([("ses_running".into(), LiveStatus::Busy)]),
                HashMap::from([("ses_running".into(), 90_000)]),
            );
            assert_eq!(state_of(this, "ses_idle"), SubagentState::Done);
            assert_eq!(state_of(this, "ses_running"), SubagentState::Working);
            assert_eq!(
                state_of(this, "ses_space"),
                SubagentState::Unknown,
                "the host's snapshot does not speak for a space"
            );
            let started = this
                .session_activity
                .started_at(&"ses_running".into())
                .expect("the host reported the start");
            assert!(now_millis() - started >= 90_000);

            // Archiving and restoring discards the status the snapshot gave.
            let archive = |archived: Option<i64>| ServerEvent::SessionPatched {
                session_id: "ses_idle".into(),
                patch: SessionPatch {
                    archived: Some(archived),
                    ..SessionPatch::default()
                },
            };
            this.apply_server_events(vec![archive(Some(5))], window, cx);
            assert!(states(this).iter().all(|(id, _)| id != "ses_idle"));
            this.apply_server_events(vec![archive(None)], window, cx);
            assert_eq!(state_of(this, "ses_idle"), SubagentState::Unknown);
            this.apply_server_events(vec![status("ses_idle", LiveStatus::Idle)], window, cx);
            assert_eq!(state_of(this, "ses_idle"), SubagentState::Done);
        });
    }

    #[gpui_kit::test]
    fn cost_follows_usage_updates_through_the_subtree(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let parent = select_parent(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            let direct = child("ses_direct", &parent, 10);
            let nested = child("ses_nested", &direct, 20);
            this.apply_server_events(created(vec![direct, nested]), window, cx);
            let cost = |this: &Workspace| this.subagent_rows()[0].cost;
            assert_eq!(cost(this), None);
            let usage = |id: &str, dollars: f64, updated: i64| ServerEvent::SessionPatched {
                session_id: id.into(),
                patch: SessionPatch {
                    updated: Some(updated),
                    cost: Usd::from_dollars(dollars),
                    ..SessionPatch::default()
                },
            };
            this.apply_server_events(
                vec![usage("ses_direct", 0.25, 30), usage("ses_nested", 0.5, 31)],
                window,
                cx,
            );
            assert_eq!(this.subagent_rows().len(), 1, "a grandchild has no row");
            assert_eq!(cost(this), Usd::from_dollars(0.75));
        });
    }

    #[gpui_kit::test]
    fn folding_is_remembered_and_only_new_children_reopen_the_section(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let parent = select_parent(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.apply_server_events(created(vec![child("ses_one", &parent, 10)]), window, cx);
        });
        assert!(shown(window, "work-status-subagent-ses_one", cx));

        click(window, "work-status-subagents", cx);
        assert!(!shown(window, "work-status-subagent-ses_one", cx));
        assert!(shown(window, "work-status-subagents", cx));

        // Another child arriving leaves a folded section folded.
        update(window, &workspace, cx, |this, window, cx| {
            this.apply_server_events(created(vec![child("ses_two", &parent, 20)]), window, cx);
        });
        assert!(!shown(window, "work-status-subagent-ses_two", cx));

        // The list empties, then a child appears: the section opens itself.
        update(window, &workspace, cx, |this, window, cx| {
            this.apply_server_events(deleted(&["ses_one", "ses_two"]), window, cx);
        });
        assert!(!shown(window, "work-status-subagents", cx));
        update(window, &workspace, cx, |this, window, cx| {
            this.apply_server_events(created(vec![child("ses_three", &parent, 30)]), window, cx);
        });
        assert!(shown(window, "work-status-subagent-ses_three", cx));
        cx.run_until_parked();
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            assert!(workspace.subagents_expanded(cx));
            assert!(
                !workspace.subagents_reopened.get(),
                "the reopening reached the stored state"
            );
            assert_eq!(
                cx.global::<UiMemory>()
                    .work_status_section_expanded(SECTION_ID),
                Some(true)
            );
        });
        assert!(shown(window, "work-status-subagent-ses_three", cx));
    }

    /// Leaves a folded, empty section, then lets one child appear, drawing
    /// the frame on which the section opens itself.
    fn reopen_a_folded_section(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        parent: &SessionRecord,
        cx: &mut TestAppContext,
    ) {
        update(window, workspace, cx, |this, window, cx| {
            this.apply_server_events(created(vec![child("ses_one", parent, 10)]), window, cx);
        });
        click(window, "work-status-subagents", cx);
        cx.run_until_parked();
        update(window, workspace, cx, |this, window, cx| {
            this.apply_server_events(deleted(&["ses_one"]), window, cx);
        });
        assert!(!shown(window, "work-status-subagents", cx));
        update(window, workspace, cx, |this, window, cx| {
            this.apply_server_events(created(vec![child("ses_two", parent, 20)]), window, cx);
        });
    }

    fn stored_fold(cx: &mut TestAppContext) -> Option<bool> {
        cx.read(|cx| {
            cx.global::<UiMemory>()
                .work_status_section_expanded(SECTION_ID)
        })
    }

    #[gpui_kit::test]
    fn a_click_on_the_frame_the_section_opened_itself_folds_it(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let parent = select_parent(window, &workspace, cx);
        reopen_a_folded_section(window, &workspace, &parent, cx);
        // No wait here: the reopening's stored write may not have run.
        click(window, "work-status-subagents", cx);
        assert!(
            !shown(window, "work-status-subagent-ses_two", cx),
            "the click folds the rows that were on screen"
        );
        // The reopening's own write, arriving late, changes nothing.
        cx.run_until_parked();
        assert!(!shown(window, "work-status-subagent-ses_two", cx));
        assert_eq!(stored_fold(cx), Some(false));
        cx.read(|cx| assert!(!workspace.read(cx).subagents_reopened.get()));

        click(window, "work-status-subagents", cx);
        assert!(shown(window, "work-status-subagent-ses_two", cx));
        assert_eq!(stored_fold(cx), Some(true));
    }

    #[gpui_kit::test]
    fn the_toggle_flips_what_is_drawn_while_the_stored_state_still_says_folded(
        cx: &mut TestAppContext,
    ) {
        let (window, workspace) = open(cx);
        let parent = select_parent(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.apply_server_events(created(vec![child("ses_one", &parent, 10)]), window, cx);
            // The state of the frame the section opens itself on, before
            // its stored write: drawn expanded over a stored fold.
            ui_memory::set_work_status_section_expanded(SECTION_ID, false, cx);
            this.subagents_reopened.set(true);
        });
        assert!(shown(window, "work-status-subagent-ses_one", cx));

        click(window, "work-status-subagents", cx);
        assert!(!shown(window, "work-status-subagent-ses_one", cx));
        assert_eq!(stored_fold(cx), Some(false));
        cx.read(|cx| assert!(!workspace.read(cx).subagents_reopened.get()));
    }

    #[gpui_kit::test]
    fn a_click_after_the_reopening_was_stored_folds_it(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let parent = select_parent(window, &workspace, cx);
        reopen_a_folded_section(window, &workspace, &parent, cx);
        cx.run_until_parked();
        assert_eq!(stored_fold(cx), Some(true));
        assert!(shown(window, "work-status-subagent-ses_two", cx));

        click(window, "work-status-subagents", cx);
        assert!(!shown(window, "work-status-subagent-ses_two", cx));
        assert_eq!(stored_fold(cx), Some(false));
    }

    #[gpui_kit::test]
    fn a_reread_withdraws_idle_certification_until_its_snapshot_applies(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let parent = select_parent(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            let mut events = created(vec![
                child("ses_quiet", &parent, 10),
                child("ses_settled", &parent, 20),
                child("ses_failed", &parent, 30),
                child("ses_running", &parent, 40),
            ]);
            events.extend([
                status("ses_settled", LiveStatus::Busy),
                status("ses_settled", LiveStatus::Idle),
                edge("ses_failed", ExecutionEdge::Failed),
                status("ses_failed", LiveStatus::Idle),
            ]);
            this.apply_server_events(events, window, cx);
            this.apply_status_snapshot(
                HashMap::from([("ses_running".into(), LiveStatus::Busy)]),
                HashMap::new(),
            );
            assert_eq!(state_of(this, "ses_quiet"), SubagentState::Done);

            // A new connection or missed events: everything is reread, and
            // the earlier snapshot stops speaking for the quiet child.
            this.reconcile(window, cx);
            assert_eq!(state_of(this, "ses_quiet"), SubagentState::Unknown);
            assert_eq!(
                state_of(this, "ses_settled"),
                SubagentState::Done,
                "an observed end stays"
            );
            assert_eq!(state_of(this, "ses_failed"), SubagentState::Failed);
            assert_eq!(state_of(this, "ses_running"), SubagentState::Working);

            // The reread's snapshot restores coverage.
            this.apply_status_snapshot(HashMap::new(), HashMap::new());
            assert_eq!(state_of(this, "ses_quiet"), SubagentState::Done);
            assert_eq!(state_of(this, "ses_failed"), SubagentState::Failed);
        });
    }

    #[gpui_kit::test]
    fn activating_a_row_opens_the_child(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let parent = select_parent(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            this.apply_server_events(created(vec![child("ses_one", &parent, 10)]), window, cx);
        });
        click(window, "work-status-subagent-ses_one", cx);
        cx.read(|cx| {
            assert_eq!(
                workspace.read(cx).selected,
                Some(SessionId::from("ses_one"))
            )
        });
    }

    #[gpui_kit::test]
    fn a_runtime_switch_clears_observations_and_coverage(cx: &mut TestAppContext) {
        let (window, workspace) = open(cx);
        let parent = select_parent(window, &workspace, cx);
        update(window, &workspace, cx, |this, window, cx| {
            let mut events = created(vec![child("ses_one", &parent, 10)]);
            events.extend([
                edge("ses_one", ExecutionEdge::Started),
                status("ses_one", LiveStatus::Busy),
                edge("ses_one", ExecutionEdge::Failed),
                status("ses_one", LiveStatus::Idle),
            ]);
            this.apply_server_events(events, window, cx);
            this.apply_status_snapshot(HashMap::new(), HashMap::new());
            assert_eq!(state_of(this, "ses_one"), SubagentState::Failed);
            assert!(this.session_activity.snapshot_covers(&"ses_one".into()));

            this.switch_runtime(RuntimeId::from("another"), window, cx);
            assert!(this.subagent_rows().is_empty());
            assert!(!this.session_activity.snapshot_covers(&"ses_one".into()));
            assert_eq!(this.session_activity.outcome(&"ses_one".into()), None);
            assert_eq!(this.subagents_present.get(), None);
        });
        assert!(!shown(window, "work-status-subagents", cx));
    }
}
