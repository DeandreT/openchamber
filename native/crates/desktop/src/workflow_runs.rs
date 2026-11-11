//! The workflow runs of the sessions on screen and the user's approve,
//! decline, stop and resume (`lib/workflows/store.ts`,
//! `useWorkflowRunAction`).
//!
//! A session's runs are read once, when the work status card first shows the
//! session; after that the control stream's `openchamber:workflow-run` events
//! keep each run current. Those events are not replayed, so every control
//! connection reads the asked sessions again. A failed read keeps the runs
//! held and is tried again the next time the card shows the session. Nothing
//! is read while workflows are not shown (`selectWorkflowsShown`).

use std::collections::{HashMap, HashSet};

use gpui_kit::{Context, SharedString, Task, Window};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::workflow_runs::{
    RunsStatus, WorkflowRun, WorkflowRunAction, WorkflowRunId, WorkflowRunStore,
};
use openchamber_native_transport::workflows::WorkflowActionError;

use crate::workspace::Workspace;

/// The runtime's workflow runs and what the card remembers about them.
#[derive(Default)]
pub(crate) struct WorkflowRuns {
    store: WorkflowRunStore,
    /// The newest read of each session; dropping one cancels its request.
    reads: HashMap<SessionId, Task<()>>,
    /// The runs with an action in flight. A run's buttons hold until the
    /// server answers, and no second action goes out for it meanwhile.
    pending: HashSet<WorkflowRunId>,
    /// The session the card last asked about. A failed read is tried again
    /// only when this changes, never on every frame.
    asked: Option<SessionId>,
    /// The runs whose fold the user flipped: an open run shows its phases
    /// and a finished one folds to its row until asked.
    toggled: HashSet<WorkflowRunId>,
}

impl WorkflowRuns {
    /// Whether an action of `run` is in flight.
    pub(crate) fn is_pending(&self, run: &WorkflowRunId) -> bool {
        self.pending.contains(run)
    }

    /// Holds or frees `run`'s actions without a request, for tests of what
    /// the card draws and accepts meanwhile.
    #[cfg(test)]
    pub(crate) fn set_pending(&mut self, run: &WorkflowRunId, pending: bool) {
        if pending {
            self.pending.insert(run.clone());
        } else {
            self.pending.remove(run);
        }
    }

    /// Whether the user flipped `run`'s fold.
    pub(crate) fn is_toggled(&self, run: &WorkflowRunId) -> bool {
        self.toggled.contains(run)
    }

    /// Flips `run`'s fold.
    pub(crate) fn toggle_run(&mut self, run: &WorkflowRunId) {
        if !self.toggled.remove(run) {
            self.toggled.insert(run.clone());
        }
    }
}

impl Workspace {
    /// Whether the app shows workflows: a server backs them and the user
    /// turned the workflow tool on.
    pub(crate) fn workflows_shown(&self) -> bool {
        !self.is_synthetic()
            && self
                .send_catalog
                .settings
                .as_ref()
                .is_some_and(|settings| settings.workflows_shown)
    }

    /// The runs `session` proposed, oldest first; empty while workflows are
    /// not shown (`useSessionWorkflowRuns`).
    pub(crate) fn session_workflow_runs(&self, session: &SessionId) -> &[WorkflowRun] {
        if self.workflows_shown() {
            self.workflow_runs.store.runs(session)
        } else {
            &[]
        }
    }

    /// The sessions of the agents of every run `parent` proposed. The
    /// Subagents section leaves these out, since the Workflows section lists
    /// them; empty while workflows are not shown.
    pub(crate) fn workflow_agent_sessions(&self, parent: &SessionId) -> HashSet<SessionId> {
        if self.workflows_shown() {
            self.workflow_runs.store.agent_session_ids(parent)
        } else {
            HashSet::new()
        }
    }

    /// Reads the selected session's runs when the work status card starts
    /// showing it. Called while rendering, so every way the selection or the
    /// card changes is covered.
    pub(crate) fn ensure_workflow_runs(&mut self, window: &Window, cx: &mut Context<Self>) {
        let card_shown = self.work_status_inline(window) || self.work_status_overlay;
        let session = (card_shown && self.workflows_shown())
            .then(|| self.selected.clone())
            .flatten();
        if self.workflow_runs.asked == session {
            return;
        }
        let asked = match &session {
            Some(session) if self.workflow_runs.store.needs_load(session) => {
                self.load_workflow_runs(session, cx)
            }
            _ => true,
        };
        // Without a connection nothing was asked; the next frame tries again.
        if asked {
            self.workflow_runs.asked = session;
        }
    }

    /// Starts a read of `session`'s runs, superseding an older one. Returns
    /// whether a request went out.
    pub(crate) fn load_workflow_runs(
        &mut self,
        session: &SessionId,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(connection) = self.connection.as_ref() else {
            return false;
        };
        let Some(client) = connection.client.clone() else {
            return false;
        };
        let runtime = self.runtime.clone();
        let ticket = self.workflow_runs.store.begin_load(session);
        let id = session.clone();
        let read = connection
            .network
            .run(async move { client.workflow_runs(&id).await });
        let task = cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                if this.runtime != runtime {
                    return;
                }
                let fetched = match result {
                    Some(Ok(runs)) => Some(runs),
                    Some(Err(error)) => {
                        tracing::warn!(%error, "the workflow runs could not be read");
                        None
                    }
                    None => None,
                };
                if this.workflow_runs.store.finish_load(&ticket, fetched) {
                    cx.notify();
                }
            });
        });
        self.workflow_runs.reads.insert(session.clone(), task);
        true
    }

    /// A run the control stream announced. When it lands on the session the
    /// card shows and that session's read had failed, the event alone would
    /// stand for the whole list, so the session is read again.
    pub(crate) fn apply_workflow_run(&mut self, run: WorkflowRun, cx: &mut Context<Self>) {
        let parent = run.parent_session_id.clone();
        if !self.workflow_runs.store.apply(run) {
            return;
        }
        cx.notify();
        if self.workflow_runs.asked.as_ref() == Some(&parent)
            && self.workflow_runs.store.status(&parent) == RunsStatus::Failed
            && self.workflows_shown()
        {
            self.load_workflow_runs(&parent, cx);
        }
    }

    /// A deletion the control stream announced.
    pub(crate) fn remove_workflow_run(
        &mut self,
        parent: &SessionId,
        run: &WorkflowRunId,
        cx: &mut Context<Self>,
    ) {
        if self.workflow_runs.store.remove(parent, run) {
            cx.notify();
        }
    }

    /// Reads every asked session again on a control connection: run events
    /// during the gap were not replayed. While workflows are not shown the
    /// sessions are marked unread instead, so they are read once workflows
    /// are turned on.
    pub(crate) fn reload_workflow_runs(&mut self, cx: &mut Context<Self>) {
        if !self.workflows_shown() {
            self.workflow_runs.store.mark_unread();
            self.workflow_runs.reads.clear();
            self.workflow_runs.asked = None;
            return;
        }
        for session in self.workflow_runs.store.loaded_sessions() {
            self.load_workflow_runs(&session, cx);
        }
    }

    /// Forgets every run, as a new runtime must. The section keeps the way
    /// the user left it folded.
    pub(crate) fn reset_workflow_runs(&mut self) {
        self.workflow_runs.store.reset();
        self.workflow_runs.reads.clear();
        self.workflow_runs.pending.clear();
        self.workflow_runs.asked = None;
        self.workflow_runs.toggled.clear();
    }

    /// Opens a run agent's session. Leaving the card for a session closes
    /// the card where it floats over the chat (`leaveWorkStatus`).
    pub(crate) fn open_workflow_agent(
        &mut self,
        session: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.work_status_overlay = false;
        self.open_subtask(session, window, cx);
        cx.notify();
    }

    /// Sends the user's approve, decline, stop or resume. The run's new
    /// state arrives as an event; a refusal shows the server's reason. While
    /// an action of the run is in flight a second one is not sent.
    pub(crate) fn act_on_workflow_run(
        &mut self,
        run: &WorkflowRunId,
        action: WorkflowRunAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        const FAILED: &str = "chat.workflow.actionFailed";
        if self.workflow_runs.is_pending(run) {
            return;
        }
        let runtime = self.runtime.clone();
        let (sent, finished) = (run.clone(), run.clone());
        let started = self.spawn_session_task(
            move |client| Box::pin(async move { client.workflow_run_action(&sent, action).await }),
            move |this, result, window, cx| {
                if this.runtime != runtime {
                    return;
                }
                this.workflow_runs.pending.remove(&finished);
                match result {
                    Some(Ok(())) => {}
                    Some(Err(WorkflowActionError::Refused { message, .. })) => {
                        this.push_error(FAILED, message.map(SharedString::from), window, cx);
                    }
                    Some(Err(WorkflowActionError::Transport(error))) => {
                        tracing::warn!(%error, "the workflow action did not reach the server");
                        this.push_error(FAILED, None, window, cx);
                    }
                    None => this.push_error(FAILED, None, window, cx),
                }
            },
            window,
            cx,
        );
        if started {
            self.workflow_runs.pending.insert(run.clone());
            cx.notify();
        } else {
            self.push_error(FAILED, None, window, cx);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext};
    use openchamber_native_core::ids::RuntimeId;
    use openchamber_native_core::workflow_runs::WorkflowRunState;
    use openchamber_native_transport::client::{ServerClient, ServerSettings};
    use openchamber_native_transport::credential::Credential;
    use serde_json::{Value, json};

    use crate::connection::{Connection, ServerConfig};
    use crate::settings_test_server::{Server, settle};

    use super::*;

    pub(crate) fn summary(id: &str, state: &str, created_at: i64) -> Value {
        json!({
            "id": id, "parentSessionId": "ses_parent", "directory": "/repo", "name": id,
            "description": "", "state": state, "attempt": 1, "createdAt": created_at,
            "currentPhase": null, "phases": [],
            "agents": [{"key": "a", "label": "Agent", "phase": null, "state": "running",
                "sessionId": format!("ses_agent_{id}")}],
            "logs": []
        })
    }

    /// A workspace connected to `server` with workflows shown.
    pub(crate) fn connect(
        server: &Server,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        attach(server, window, &workspace, cx);
        (window, workspace)
    }

    /// Connects an open workspace to `server` and shows workflows.
    pub(crate) fn attach(
        server: &Server,
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) {
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, _| {
                let mut connection =
                    Connection::new(ServerConfig::for_endpoint(server.endpoint.clone()))
                        .expect("network");
                connection.client = Some(
                    ServerClient::new(server.endpoint.clone(), Credential::None)
                        .expect("fixture client"),
                );
                workspace.connection = Some(connection);
                workspace.send_catalog.settings = Some(ServerSettings {
                    workflows_shown: true,
                    ..ServerSettings::default()
                });
            });
        })
        .expect("test window");
    }

    fn event_run(id: &str, state: WorkflowRunState) -> WorkflowRun {
        WorkflowRun {
            id: WorkflowRunId::from(id),
            parent_session_id: SessionId::from("ses_parent"),
            directory: "/repo".to_owned(),
            name: id.to_owned(),
            description: String::new(),
            state,
            created_at: 2,
            current_phase: None,
            phases: Vec::new(),
            agents: Vec::new(),
            error: None,
        }
    }

    fn reads_of(server: &Server, session: &str) -> usize {
        let query = format!("sessionId={session}");
        server
            .requests()
            .iter()
            .filter(|request| {
                request.is("GET", "/api/openchamber/workflows") && request.target.contains(&query)
            })
            .count()
    }

    fn load(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        session: &SessionId,
        cx: &mut TestAppContext,
    ) {
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                assert!(workspace.load_workflow_runs(session, cx));
            });
        })
        .expect("test window");
    }

    fn run_ids(
        workspace: &Entity<Workspace>,
        session: &SessionId,
        cx: &mut TestAppContext,
    ) -> Vec<String> {
        cx.read(|cx| {
            workspace
                .read(cx)
                .session_workflow_runs(session)
                .iter()
                .map(|run| run.id.to_string())
                .collect()
        })
    }

    fn status(
        workspace: &Entity<Workspace>,
        session: &SessionId,
        cx: &mut TestAppContext,
    ) -> RunsStatus {
        cx.read(|cx| workspace.read(cx).workflow_runs.store.status(session))
    }

    #[gpui_kit::test]
    fn runs_load_stay_through_a_failed_reread_and_follow_events(cx: &mut TestAppContext) {
        let failing = Arc::new(AtomicBool::new(false));
        let fail = failing.clone();
        let server = Server::new(move |request| {
            if !request.is("GET", "/api/openchamber/workflows") {
                return (404, json!({"error": "unknown route"}));
            }
            if fail.load(Ordering::Acquire) {
                return (503, json!({"error": "starting"}));
            }
            if request.target.contains("sessionId=ses_empty") {
                return (200, json!({"runs": []}));
            }
            (
                200,
                json!({"runs": [summary("run_a", "completed", 1), summary("run_b", "running", 2)]}),
            )
        });
        let (window, workspace) = connect(&server, cx);
        let (parent, empty) = (SessionId::from("ses_parent"), SessionId::from("ses_empty"));

        load(window, &workspace, &parent, cx);
        load(window, &workspace, &empty, cx);
        settle(window, cx);
        assert_eq!(run_ids(&workspace, &parent, cx), ["run_a", "run_b"]);
        assert_eq!(status(&workspace, &empty, cx), RunsStatus::Ready);
        assert!(run_ids(&workspace, &empty, cx).is_empty());
        let members = cx.read(|cx| workspace.read(cx).workflow_agent_sessions(&parent));
        assert_eq!(
            members,
            HashSet::from([
                SessionId::from("ses_agent_run_a"),
                SessionId::from("ses_agent_run_b")
            ])
        );

        // A reconnect rereads the asked sessions; its failure keeps the rows.
        failing.store(true, Ordering::Release);
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| workspace.reload_workflow_runs(cx));
        })
        .expect("test window");
        settle(window, cx);
        assert_eq!(status(&workspace, &parent, cx), RunsStatus::Failed);
        assert_eq!(run_ids(&workspace, &parent, cx), ["run_a", "run_b"]);
        assert_eq!(
            server
                .requests()
                .iter()
                .filter(|request| request.target.contains("sessionId=ses_parent"))
                .count(),
            2,
            "one read and one reread, no retry loop"
        );

        // Events replace one run and delete another without touching the rest.
        let changed = WorkflowRun {
            state: WorkflowRunState::Stopped,
            ..cx.read(|cx| workspace.read(cx).session_workflow_runs(&parent)[1].clone())
        };
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.apply_workflow_run(changed, cx);
                workspace.remove_workflow_run(&parent, &WorkflowRunId::from("run_a"), cx);
            });
        })
        .expect("test window");
        assert_eq!(run_ids(&workspace, &parent, cx), ["run_b"]);
    }

    #[gpui_kit::test]
    fn a_failed_first_read_is_failed_not_empty(cx: &mut TestAppContext) {
        let server = Server::new(|_| (500, json!({"error": "boom"})));
        let (window, workspace) = connect(&server, cx);
        let parent = SessionId::from("ses_parent");
        load(window, &workspace, &parent, cx);
        settle(window, cx);
        assert_eq!(status(&workspace, &parent, cx), RunsStatus::Failed);
        assert!(run_ids(&workspace, &parent, cx).is_empty());
    }

    #[gpui_kit::test]
    fn a_runtime_switch_discards_the_runs_and_a_read_on_its_way(cx: &mut TestAppContext) {
        let server = Server::new(|_| (200, json!({"runs": [summary("run_a", "running", 1)]})));
        let (window, workspace) = connect(&server, cx);
        let parent = SessionId::from("ses_parent");
        load(window, &workspace, &parent, cx);
        settle(window, cx);
        assert_eq!(run_ids(&workspace, &parent, cx), ["run_a"]);

        // A second read is on its way while the runtime changes.
        load(window, &workspace, &parent, cx);
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, _| {
                workspace.runtime = RuntimeId::from("another-runtime");
                workspace.reset_workflow_runs();
            });
        })
        .expect("test window");
        settle(window, cx);
        assert_eq!(status(&workspace, &parent, cx), RunsStatus::Idle);
        assert!(
            cx.read(|cx| workspace
                .read(cx)
                .workflow_runs
                .store
                .runs(&parent)
                .is_empty()),
            "the previous runtime's runs and its late answer are gone"
        );
    }

    #[gpui_kit::test]
    fn nothing_is_read_or_listed_while_workflows_are_not_shown(cx: &mut TestAppContext) {
        let server = Server::new(|_| (200, json!({"runs": [summary("run_a", "running", 1)]})));
        let (window, workspace) = connect(&server, cx);
        let parent = SessionId::from("ses_parent");
        load(window, &workspace, &parent, cx);
        settle(window, cx);
        let reads = |server: &Server| {
            server
                .requests()
                .iter()
                .filter(|request| request.is("GET", "/api/openchamber/workflows"))
                .count()
        };
        let before = reads(&server);
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.send_catalog.settings = Some(ServerSettings::default());
                workspace.reload_workflow_runs(cx);
            });
        })
        .expect("test window");
        settle(window, cx);
        assert!(run_ids(&workspace, &parent, cx).is_empty());
        assert!(
            cx.read(|cx| workspace
                .read(cx)
                .workflow_agent_sessions(&parent)
                .is_empty()),
            "no run means no membership"
        );
        assert_eq!(status(&workspace, &parent, cx), RunsStatus::Idle);
        assert_eq!(
            reads(&server),
            before,
            "turned off, neither the reconnect nor the card reads anything"
        );
    }

    #[gpui_kit::test]
    fn an_action_holds_its_run_until_answered_and_a_refusal_changes_no_run(
        cx: &mut TestAppContext,
    ) {
        let posted = Arc::new(Mutex::new(Vec::new()));
        let log = posted.clone();
        let server = Server::new(move |request| {
            if request.method == "POST" {
                log.lock().expect("log").push(request.target.clone());
                return if request.target.ends_with("/resume") {
                    (409, json!({"error": "The run is already going"}))
                } else {
                    (200, json!({"ok": true}))
                };
            }
            (
                200,
                json!({"runs": [summary("run_a", "awaiting-approval", 1), summary("run_b", "failed", 2)]}),
            )
        });
        let (window, workspace) = connect(&server, cx);
        let parent = SessionId::from("ses_parent");
        load(window, &workspace, &parent, cx);
        settle(window, cx);

        let act = |run: &'static str, action: WorkflowRunAction, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.act_on_workflow_run(&WorkflowRunId::from(run), action, window, cx);
                    assert!(
                        workspace
                            .workflow_runs
                            .is_pending(&WorkflowRunId::from(run))
                    );
                });
            })
            .expect("test window");
        };
        act("run_a", WorkflowRunAction::Approve, cx);
        settle(window, cx);
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            assert!(
                !workspace
                    .workflow_runs
                    .is_pending(&WorkflowRunId::from("run_a"))
            );
            assert!(
                workspace.session_management.notice.is_none(),
                "an accepted action says nothing; the new state arrives as an event"
            );
        });

        act("run_b", WorkflowRunAction::Resume, cx);
        settle(window, cx);
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            assert!(
                !workspace
                    .workflow_runs
                    .is_pending(&WorkflowRunId::from("run_b"))
            );
            assert!(
                workspace.session_management.notice.is_some(),
                "a refusal is reported"
            );
        });
        assert_eq!(run_ids(&workspace, &parent, cx), ["run_a", "run_b"]);
        assert_eq!(
            *posted.lock().expect("log"),
            [
                "/api/openchamber/workflows/run_a/approve",
                "/api/openchamber/workflows/run_b/resume"
            ]
        );
    }

    #[gpui_kit::test]
    fn a_second_action_waits_for_the_runs_first_and_other_runs_are_free(cx: &mut TestAppContext) {
        let posted = Arc::new(Mutex::new(Vec::new()));
        let log = posted.clone();
        let server = Server::new(move |request| {
            if request.method == "POST" {
                log.lock().expect("log").push(request.target.clone());
            }
            (200, json!({"runs": []}))
        });
        let (window, workspace) = connect(&server, cx);
        let (first, second) = (WorkflowRunId::from("run_a"), WorkflowRunId::from("run_b"));
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.act_on_workflow_run(&first, WorkflowRunAction::Approve, window, cx);
                // The first is still on its way: neither a repeat nor the
                // opposite choice goes out for the same run.
                workspace.act_on_workflow_run(&first, WorkflowRunAction::Approve, window, cx);
                workspace.act_on_workflow_run(&first, WorkflowRunAction::Decline, window, cx);
                workspace.act_on_workflow_run(&second, WorkflowRunAction::Resume, window, cx);
                assert!(workspace.workflow_runs.is_pending(&first));
                assert!(
                    workspace.workflow_runs.is_pending(&second),
                    "another run's action does not free or block this one"
                );
            });
        })
        .expect("test window");
        settle(window, cx);
        let mut sent = posted.lock().expect("log").clone();
        sent.sort();
        assert_eq!(
            sent,
            [
                "/api/openchamber/workflows/run_a/approve",
                "/api/openchamber/workflows/run_b/resume"
            ]
        );
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                assert!(!workspace.workflow_runs.is_pending(&first));
                assert!(!workspace.workflow_runs.is_pending(&second));
                // Answered, the run takes the next action.
                workspace.act_on_workflow_run(&first, WorkflowRunAction::Decline, window, cx);
            });
        })
        .expect("test window");
        settle(window, cx);
        assert_eq!(posted.lock().expect("log").len(), 3);
    }

    #[gpui_kit::test]
    fn an_event_on_the_shown_session_whose_read_failed_reads_it_again(cx: &mut TestAppContext) {
        let failing = Arc::new(AtomicBool::new(true));
        let fail = failing.clone();
        let server = Server::new(move |request| {
            if !request.is("GET", "/api/openchamber/workflows") {
                return (404, json!({"error": "unknown route"}));
            }
            if fail.load(Ordering::Acquire) {
                return (503, json!({"error": "starting"}));
            }
            (
                200,
                json!({"runs": [summary("run_a", "completed", 1), summary("run_b", "running", 2)]}),
            )
        });
        let (window, workspace) = connect(&server, cx);
        let parent = SessionId::from("ses_parent");
        load(window, &workspace, &parent, cx);
        settle(window, cx);
        assert_eq!(status(&workspace, &parent, cx), RunsStatus::Failed);
        assert_eq!(reads_of(&server, "ses_parent"), 1);
        failing.store(false, Ordering::Release);

        // The card shows another session: the event is kept and nothing is read.
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.workflow_runs.asked = None;
                workspace.apply_workflow_run(event_run("run_b", WorkflowRunState::Running), cx);
            });
        })
        .expect("test window");
        settle(window, cx);
        assert_eq!(reads_of(&server, "ses_parent"), 1);
        assert_eq!(status(&workspace, &parent, cx), RunsStatus::Failed);

        // The card shows this session: one run from an event is not the list.
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.workflow_runs.asked = Some(parent.clone());
                workspace.apply_workflow_run(event_run("run_b", WorkflowRunState::Running), cx);
                assert_eq!(
                    workspace.workflow_runs.store.status(&parent),
                    RunsStatus::Loading
                );
            });
        })
        .expect("test window");
        settle(window, cx);
        assert_eq!(reads_of(&server, "ses_parent"), 2);
        assert_eq!(status(&workspace, &parent, cx), RunsStatus::Ready);
        assert_eq!(run_ids(&workspace, &parent, cx), ["run_a", "run_b"]);

        // Read, later events read nothing more.
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.workflow_runs.asked = Some(parent.clone());
                workspace.apply_workflow_run(event_run("run_b", WorkflowRunState::Completed), cx);
            });
        })
        .expect("test window");
        settle(window, cx);
        assert_eq!(reads_of(&server, "ses_parent"), 2);
    }

    #[gpui_kit::test]
    fn opening_an_agent_from_the_floating_card_closes_the_card(cx: &mut TestAppContext) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let agent = workspace
                    .sessions
                    .active()
                    .first()
                    .map(|session| session.id.clone())
                    .expect("the synthetic source lists sessions");
                workspace.work_status_overlay = true;
                workspace.open_workflow_agent(agent, window, cx);
                assert!(
                    !workspace.work_status_overlay,
                    "leaving for a session closes the floating card"
                );
            });
        })
        .expect("test window");
    }
}
