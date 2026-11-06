//! Headless goal and review-loop tests with real journal workers and a
//! scripted server that keeps session metadata, the goal objective file and
//! auto-review runs the way the OpenChamber server does.

use std::collections::{HashMap, HashSet};

use openchamber_native_core::auto_review::{AutoReviewRun, RunStatus, RunStep};
use openchamber_native_core::ids::SessionId;
use openchamber_native_core::session_goal::GoalStatus;
use openchamber_native_transport::event::ServerEvent;
use openchamber_native_transport::session_goal::GoalStatusChange;
use serde_json::{Map, json};

use super::*;

/// What the scripted server holds and how it fails.
#[derive(Default)]
struct State {
    /// Session metadata by session ID.
    metadata: HashMap<String, Value>,
    objectives: HashMap<String, String>,
    /// `METHOD path-prefix` answered with a 500.
    fail: HashSet<String>,
    /// The runs `GET /api/auto-review` reports, and the revision.
    runs: Vec<Value>,
    revision: u64,
}

struct Scripted {
    endpoint: Endpoint,
    requests: Arc<Mutex<Vec<Request>>>,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

fn merge(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let target = target.as_object_mut().expect("object");
    for (key, value) in patch {
        if value.is_null() {
            target.remove(key);
        } else {
            merge(target.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

fn session_json(id: &str, metadata: &Value) -> Value {
    json!({ "id": id, "projectID": "global", "location": { "directory": "/project" },
        "title": "Goal test", "cost": 0,
        "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
        "time": { "created": 1, "updated": 1 }, "metadata": metadata })
}

fn answer(request: &Request, state: &mut State) -> (&'static str, String) {
    let path = request.path.split('?').next().unwrap_or_default();
    let method = request.method.as_str();
    if state
        .fail
        .iter()
        .any(|failing| format!("{method} {path}").starts_with(failing))
    {
        return ("500 Internal Server Error", "{}".to_owned());
    }
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let ok = |value: Value| ("200 OK", value.to_string());
    match (method, segments.as_slice()) {
        ("GET" | "PUT", ["api", "config", "settings"]) => {
            let echo = if method == "PUT" { body } else { json!({}) };
            ok(echo)
        }
        ("GET", ["api", "small-model"]) => ok(json!({ "available": true })),
        ("GET", ["api", "magic-prompts"]) => ok(json!({ "version": 1, "overrides": {} })),
        ("PUT", ["api", "goals", "objective", id]) => {
            state.objectives.insert(
                (*id).to_owned(),
                body["content"].as_str().unwrap_or_default().to_owned(),
            );
            ok(json!({ "ok": true }))
        }
        ("GET", ["api", "goals", "objective", id]) => match state.objectives.get(*id) {
            Some(content) => ok(json!({ "content": content })),
            None => ("404 Not Found", "{}".to_owned()),
        },
        ("DELETE", ["api", "goals", "objective", id]) => {
            state.objectives.remove(*id);
            ok(json!({ "ok": true }))
        }
        ("POST", ["api", "openchamber", "sessions", id, "metadata"]) => {
            let stored = state.metadata.entry((*id).to_owned()).or_insert(json!({}));
            merge(stored, &body["patch"]);
            ok(json!({ "metadata": stored }))
        }
        ("GET", ["api", "session", id]) => {
            let metadata = state.metadata.get(*id).cloned().unwrap_or(json!({}));
            ok(json!({ "data": session_json(id, &metadata) }))
        }
        ("POST", ["api", "session"]) => {
            let id = body["id"].as_str().unwrap_or_default().to_owned();
            state.metadata.insert(id.clone(), body["metadata"].clone());
            let mut session = session_json(&id, &body["metadata"]);
            session["title"] = body["title"].clone();
            ok(json!({ "data": session }))
        }
        ("POST", ["api", "session", id, kind @ ("synthetic" | "prompt")]) => ok(json!({ "data": {
            "id": body["id"], "sessionID": id,
            "type": if *kind == "prompt" { "user" } else { "synthetic" },
            "time": { "created": 1 }, "delivery": "queue", "payload": {}
        } })),
        ("POST", ["api", "session", _, "interrupt"]) => ok(json!({ "interrupted": true })),
        ("POST", ["api", "session", _, _]) => ("204 No Content", String::new()),
        ("GET", ["api", "session", _, "inbox"]) => ok(json!({ "data": [] })),
        ("GET", ["api", "auto-review"]) => {
            ok(json!({ "revision": state.revision, "runs": state.runs }))
        }
        ("POST", ["api", "auto-review", "runs"]) => {
            state.revision += 1;
            let run = json!({
                "originalSessionID": body["originalSessionID"],
                "reviewSessionID": body["reviewSessionID"],
                "directory": body["directory"], "status": "running", "step": "handoff",
                "round": 0, "maxRounds": 5, "startedAt": 10, "updatedAt": 10
            });
            state.runs = vec![run.clone()];
            ok(json!({ "revision": state.revision, "run": run }))
        }
        ("DELETE", ["api", "auto-review", "runs", _]) => {
            state.revision += 1;
            state.runs.clear();
            ok(json!({ "revision": state.revision, "run": null }))
        }
        _ => ("404 Not Found", "{}".to_owned()),
    }
}

impl Scripted {
    fn new(state: State) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener binds");
        listener.set_nonblocking(true).expect("nonblocking");
        let endpoint = Endpoint::parse(&format!(
            "http://{}",
            listener.local_addr().expect("address")
        ))
        .expect("endpoint");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new(state));
        let stop = Arc::new(AtomicBool::new(false));
        let (recorded, scripted, stopped) = (requests.clone(), state.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("read timeout");
                let request = read_request(&mut stream);
                let (status, body) = answer(&request, &mut scripted.lock().expect("state"));
                recorded.lock().expect("requests").push(request);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            endpoint,
            requests,
            state,
            stop,
            worker: Some(worker),
        }
    }

    fn server(&self) -> Server {
        Server {
            endpoint: self.endpoint.clone(),
            requests: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(true)),
            release_prompt: Arc::new(AtomicBool::new(true)),
            worker: None,
        }
    }

    /// `METHOD path` of every request except settings reads, in order.
    fn calls(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("requests")
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .filter(|call| !call.contains("/api/config/settings") && !call.contains("small-model"))
            .collect()
    }

    fn body_of(&self, call: &str) -> Value {
        let requests = self.requests.lock().expect("requests");
        let request = requests
            .iter()
            .rfind(|request| format!("{} {}", request.method, request.path) == call)
            .unwrap_or_else(|| panic!("{call} was requested"));
        serde_json::from_slice(&request.body).expect("JSON body")
    }
}

impl Drop for Scripted {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.expect("scripted server completes");
            }
        }
    }
}

fn setup(
    cx: &mut TestAppContext,
    scripted: &Scripted,
    fixture: &Fixture,
    key: &SessionKey,
) -> (AnyWindowHandle, Entity<Workspace>) {
    let (window, workspace) = open(cx, &scripted.server(), fixture, key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, key, window, cx)
        })
    })
    .expect("window");
    (window, workspace)
}

fn update(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    action: impl FnOnce(&mut Workspace, &mut gpui_kit::Window, &mut gpui_kit::Context<Workspace>),
) {
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| action(workspace, window, cx))
    })
    .expect("window");
}

fn goal_of(
    cx: &mut TestAppContext,
    workspace: &Entity<Workspace>,
    key: &SessionKey,
) -> Option<GoalStatus> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .sessions
            .get(&key.session)
            .and_then(|session| session.goal.as_ref())
            .map(|goal| goal.status)
    })
}

#[gpui_kit::test]
fn an_armed_send_saves_the_goal_and_its_reminder_before_the_prompt(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("goal-runtime", "ses_goal");
    let scripted = Scripted::new(State::default());
    let (window, workspace) = setup(cx, &scripted, &fixture, &key);

    update(cx, window, &workspace, |workspace, window, cx| {
        workspace.press_goal_button(window, cx);
        assert!(workspace.goals.armed, "a press arms the next send");
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("Ship it", window, cx));
        workspace.submit_connected(window, cx);
        assert!(!workspace.goals.armed, "the send consumed the arm");
        assert!(
            !workspace.composer_editable(),
            "the composer waits for the goal"
        );
    });
    wait(cx, |_| {
        scripted
            .calls()
            .iter()
            .any(|call| call.ends_with("/prompt"))
    });
    let calls = scripted.calls();
    let position = |needle: &str| {
        calls
            .iter()
            .position(|call| call.contains(needle))
            .unwrap_or_else(|| panic!("{needle} in {calls:?}"))
    };
    assert!(position("PUT /api/goals/objective/ses_goal") < position("/metadata"));
    assert!(position("/metadata") < position("/synthetic"));
    assert!(position("/synthetic") < position("/prompt"));
    let metadata = scripted.body_of("POST /api/openchamber/sessions/ses_goal/metadata");
    let goal = &metadata["patch"]["openchamber"]["goal"];
    assert_eq!(
        (goal["status"].as_str(), goal["objectiveFile"].as_bool()),
        (Some("active"), Some(true))
    );
    assert_eq!(
        scripted.state.lock().expect("state").objectives["ses_goal"],
        "Ship it"
    );
    let intro = scripted.body_of("POST /api/session/ses_goal/synthetic");
    assert!(
        intro["text"]
            .as_str()
            .is_some_and(|text| text.contains("Goal mode is active"))
    );
    assert_eq!(intro["resume"], json!(false));
    assert_eq!(
        scripted.body_of("POST /api/session/ses_goal/prompt")["text"],
        "Ship it"
    );
    wait(cx, |cx| {
        goal_of(cx, &workspace, &key) == Some(GoalStatus::Active)
    });
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn a_goal_that_cannot_be_saved_sends_nothing_and_rearms(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("goal-fail-runtime", "ses_fail");
    let scripted = Scripted::new(State {
        fail: HashSet::from(["POST /api/openchamber/sessions/".to_owned()]),
        ..State::default()
    });
    let (window, workspace) = setup(cx, &scripted, &fixture, &key);
    update(cx, window, &workspace, |workspace, window, cx| {
        workspace.goals.armed = true;
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("Ship it", window, cx));
        workspace.submit_connected(window, cx);
    });
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            workspace.goals.armed && workspace.goals.arming.is_empty()
        })
    });
    assert!(
        !scripted
            .calls()
            .iter()
            .any(|call| call.ends_with("/prompt")),
        "nothing is sent without its goal"
    );
    assert_eq!(
        cx.read(|cx| workspace.read(cx).composer.read(cx).value().to_string()),
        "Ship it",
        "the text stays for another try"
    );
    assert_eq!(goal_of(cx, &workspace, &key), None);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn pause_stops_the_turn_and_resume_restarts_the_goal(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("goal-pause-runtime", "ses_pause");
    let scripted = Scripted::new(State {
        metadata: HashMap::from([(
            "ses_pause".to_owned(),
            json!({ "openchamber": { "goal": {
                "id": "g1", "objective": "Ship", "status": "blocked", "turnsUsed": 20,
                "statusReason": "turn limit"
            } } }),
        )]),
        ..State::default()
    });
    let (window, workspace) = setup(cx, &scripted, &fixture, &key);
    update(cx, window, &workspace, |workspace, window, cx| {
        workspace.change_goal_status(GoalStatusChange::Active, window, cx)
    });
    wait(cx, |cx| {
        goal_of(cx, &workspace, &key) == Some(GoalStatus::Active)
    });
    let resumed = scripted.body_of("POST /api/openchamber/sessions/ses_pause/metadata");
    assert_eq!(
        resumed["patch"]["openchamber"]["goal"]["turnsUsed"],
        json!(0),
        "Resume grants a fresh turn allowance"
    );
    update(cx, window, &workspace, |workspace, window, cx| {
        workspace.change_goal_status(GoalStatusChange::Paused, window, cx)
    });
    wait(cx, |cx| {
        goal_of(cx, &workspace, &key) == Some(GoalStatus::Paused)
    });
    assert!(
        scripted
            .calls()
            .contains(&"POST /api/session/ses_pause/interrupt".to_owned()),
        "Pause also stops the running turn"
    );
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn a_failed_status_change_keeps_the_server_goal(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("goal-status-fail", "ses_keep");
    let scripted = Scripted::new(State {
        metadata: HashMap::from([(
            "ses_keep".to_owned(),
            json!({ "openchamber": { "goal": { "id": "g1", "objective": "Ship", "status": "active" } } }),
        )]),
        fail: HashSet::from(["POST /api/openchamber/sessions/".to_owned()]),
        ..State::default()
    });
    let (window, workspace) = setup(cx, &scripted, &fixture, &key);
    // The record the window holds carries the goal the server reported.
    update(cx, window, &workspace, |workspace, window, cx| {
        workspace.apply_server_events(
            vec![ServerEvent::SessionPatched {
                session_id: key.session.clone(),
                patch: openchamber_native_core::session::SessionPatch {
                    metadata: Some(openchamber_native_core::session::MetadataFacts {
                        goal: openchamber_native_transport::session_goal::goal_of(
                            json!({ "openchamber": { "goal": { "id": "g1", "objective": "Ship", "status": "active" } } })
                                .as_object()
                                .expect("object"),
                        ),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            }],
            window,
            cx,
        );
        workspace.change_goal_status(GoalStatusChange::Paused, window, cx);
        assert!(workspace.goals.busy.contains(&key));
    });
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).goals.busy.is_empty())
    });
    assert_eq!(
        goal_of(cx, &workspace, &key),
        Some(GoalStatus::Active),
        "a failed write changes nothing until the server says so"
    );
    close(cx, window, &workspace);
}

fn running_run(original: &str) -> AutoReviewRun {
    AutoReviewRun {
        original_session: SessionId::new(original),
        review_session: SessionId::new("ses_reviewer"),
        directory: "/project".to_owned(),
        status: RunStatus::Running,
        step: RunStep::Review,
        round: 1,
        max_rounds: 5,
        started_at: 10,
        updated_at: 11,
        error: None,
        dispute: None,
    }
}

#[gpui_kit::test]
fn an_automatic_review_starts_on_a_new_linked_review_session(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("review-runtime", "ses_impl");
    let scripted = Scripted::new(State::default());
    let (window, workspace) = setup(cx, &scripted, &fixture, &key);
    update(cx, window, &workspace, |workspace, window, cx| {
        workspace.open_review_dialog(window, cx);
        workspace.toggle_review_auto(cx);
        workspace.start_review(window, cx);
        assert!(
            workspace
                .reviews
                .dialog
                .as_ref()
                .is_some_and(|dialog| dialog.submitting)
        );
    });
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            workspace.reviews.dialog.is_none() && workspace.selected_auto_review().is_some()
        })
    });
    let created = scripted.body_of("POST /api/session");
    assert_eq!(
        created["metadata"],
        json!({ "openchamber": { "kind": "review", "originalSessionID": "ses_impl" } })
    );
    assert_eq!(created["title"], "Review: Goal test");
    let review_id = created["id"].as_str().expect("review id").to_owned();
    assert_eq!(
        scripted.body_of("POST /api/openchamber/sessions/ses_impl/metadata")["patch"],
        json!({ "openchamber": { "reviewSessionID": review_id } })
    );
    let start = scripted.body_of("POST /api/auto-review/runs");
    assert_eq!(start["reviewSessionID"], json!(review_id));
    assert!(
        start["prompts"]["reviewStart"]
            .as_str()
            .is_some_and(|text| text.contains("{{handoff}}")),
        "the server fills the placeholders"
    );
    assert!(start["handoff"]["text"].is_string());
    assert_eq!(
        cx.read(|cx| workspace
            .read(cx)
            .selected_auto_review()
            .map(|run| run.step)),
        Some(RunStep::Handoff)
    );
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn stopping_a_review_removes_the_row_and_a_failure_brings_it_back(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("review-stop-runtime", "ses_stop");
    let scripted = Scripted::new(State::default());
    let (window, workspace) = setup(cx, &scripted, &fixture, &key);
    let seed = |cx: &mut TestAppContext| {
        update(cx, window, &workspace, |workspace, _, _| {
            workspace.reviews.projection.apply_update(
                None,
                1,
                &SessionId::new("ses_stop"),
                "/project",
                Some(running_run("ses_stop")),
            );
        })
    };
    seed(cx);
    update(cx, window, &workspace, |workspace, window, cx| {
        workspace.remove_auto_review(window, cx);
        assert!(
            workspace.selected_auto_review().is_none(),
            "the row goes at once"
        );
    });
    wait(cx, |_| {
        scripted
            .calls()
            .contains(&"DELETE /api/auto-review/runs/ses_stop".to_owned())
    });
    cx.run_until_parked();
    assert!(cx.read(|cx| workspace.read(cx).selected_auto_review().is_none()));

    // The server keeps it: the row comes back and the owner is read again.
    scripted.state.lock().expect("state").fail =
        HashSet::from(["DELETE /api/auto-review".to_owned()]);
    {
        let mut state = scripted.state.lock().expect("state");
        state.revision = 9;
        state.runs = vec![json!({
            "originalSessionID": "ses_stop", "reviewSessionID": "ses_reviewer",
            "directory": "/project", "status": "running", "step": "implement",
            "round": 2, "maxRounds": 5, "startedAt": 10, "updatedAt": 12
        })];
    }
    seed(cx);
    update(cx, window, &workspace, |workspace, window, cx| {
        workspace.remove_auto_review(window, cx)
    });
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace
                .read(cx)
                .selected_auto_review()
                .is_some_and(|run| run.step == RunStep::Implement)
        })
    });
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn review_broadcasts_and_reconnect_reads_follow_the_server(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("review-event-runtime", "ses_event");
    let mut settled = running_run("ses_event");
    settled.status = RunStatus::Disputed;
    settled.dispute = Some("Both positions".to_owned());
    let scripted = Scripted::new(State {
        revision: 3,
        runs: vec![json!({
            "originalSessionID": "ses_event", "reviewSessionID": "ses_reviewer",
            "directory": "/project", "status": "disputed", "step": "review",
            "round": 1, "maxRounds": 5, "startedAt": 10, "updatedAt": 11,
            "dispute": "Both positions"
        })],
        ..State::default()
    });
    let (window, workspace) = setup(cx, &scripted, &fixture, &key);
    // A reconnect reads the host's runs: broadcasts are not replayed.
    update(cx, window, &workspace, |workspace, _, cx| {
        workspace.load_auto_reviews(cx)
    });
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).selected_auto_review() == Some(&settled))
    });
    update(cx, window, &workspace, |workspace, window, cx| {
        workspace.toggle_auto_review_dispute(cx);
        assert!(workspace.reviews.unfolded.is_some());
        // An older broadcast changes nothing; a newer removal clears the row.
        workspace.apply_server_events(
            vec![ServerEvent::AutoReviewUpdated(
                openchamber_native_transport::review::AutoReviewUpdate {
                    space: None,
                    revision: 2,
                    original_session: key.session.clone(),
                    directory: "/project".to_owned(),
                    run: None,
                },
            )],
            window,
            cx,
        );
        assert!(workspace.selected_auto_review().is_some());
        workspace.apply_server_events(
            vec![ServerEvent::AutoReviewUpdated(
                openchamber_native_transport::review::AutoReviewUpdate {
                    space: None,
                    revision: 4,
                    original_session: key.session.clone(),
                    directory: "/project".to_owned(),
                    run: None,
                },
            )],
            window,
            cx,
        );
        assert!(workspace.selected_auto_review().is_none());
    });
    close(cx, window, &workspace);
}

fn prepare_draft(
    workspace: &mut Workspace,
    key: &SessionKey,
    window: &mut gpui_kit::Window,
    cx: &mut gpui_kit::Context<Workspace>,
) {
    configure_send(workspace, key, window, cx);
    workspace.show_new_session(window, cx);
    workspace.choose_draft_project("/project", window, cx);
    workspace.composer.update(cx, |composer, cx| {
        composer.set_value("Build it", window, cx)
    });
    assert!(workspace.can_create_connected());
}

#[gpui_kit::test]
fn a_new_session_saves_its_goal_once_created_and_before_its_first_prompt(cx: &mut TestAppContext) {
    for fails in [false, true] {
        let fixture = Fixture::new();
        let key = tests::key("goal-create-runtime", "ses_existing");
        let scripted = Scripted::new(State {
            fail: if fails {
                HashSet::from(["POST /api/openchamber/sessions/".to_owned()])
            } else {
                HashSet::new()
            },
            ..State::default()
        });
        let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
        ready(cx, &workspace);
        update(cx, window, &workspace, |workspace, window, cx| {
            prepare_draft(workspace, &key, window, cx);
            workspace.press_goal_button(window, cx);
            workspace.submit(window, cx);
            assert!(!workspace.goals.armed);
            assert_eq!(workspace.goals.created.len(), 1);
        });
        wait(cx, |cx| {
            cx.read(|cx| {
                let workspace = workspace.read(cx);
                workspace.goals.created.is_empty() && workspace.mutation_tasks.is_empty()
            })
        });
        let calls = scripted.calls();
        let create = calls
            .iter()
            .position(|call| call == "POST /api/session")
            .expect("created");
        let goal = calls
            .iter()
            .position(|call| call.ends_with("/metadata"))
            .expect("goal write");
        assert!(create < goal, "{calls:?}");
        if fails {
            assert!(
                !calls.iter().any(|call| call.ends_with("/prompt")),
                "no first prompt without its goal: {calls:?}"
            );
            assert!(
                cx.read(|cx| workspace.read(cx).goals.armed),
                "the arm comes back with the draft"
            );
        } else {
            let prompt = calls
                .iter()
                .position(|call| call.ends_with("/prompt"))
                .expect("prompt");
            let intro = calls
                .iter()
                .position(|call| call.ends_with("/synthetic"))
                .expect("reminder");
            assert!(goal < intro && intro < prompt, "{calls:?}");
            let state = scripted.state.lock().expect("state");
            let (_, metadata) = state
                .metadata
                .iter()
                .find(|(id, _)| id.as_str() != "ses_existing")
                .expect("created session metadata");
            assert_eq!(metadata["openchamber"]["goal"]["status"], "active");
        }
        close(cx, window, &workspace);
    }
}

#[gpui_kit::test]
fn a_manual_review_without_handoff_prompts_the_review_session_and_opens_it(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("review-manual-runtime", "ses_manual");
    let scripted = Scripted::new(State::default());
    let (window, workspace) = setup(cx, &scripted, &fixture, &key);
    update(cx, window, &workspace, |workspace, window, cx| {
        workspace.composer.update(cx, |composer, cx| {
            composer.set_value("/handoff-review", window, cx)
        });
        workspace.submit(window, cx);
        assert!(
            workspace.reviews.dialog.is_some(),
            "the command opens the dialog"
        );
        assert!(workspace.composer.read(cx).value().is_empty());
        workspace.toggle_review_handoff(cx);
        workspace.start_review(window, cx);
    });
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            workspace.reviews.dialog.is_none() && workspace.selected.as_ref() != Some(&key.session)
        })
    });
    let review = scripted.body_of("POST /api/session")["id"]
        .as_str()
        .expect("review id")
        .to_owned();
    assert_eq!(
        cx.read(|cx| workspace.read(cx).selected.clone()),
        Some(SessionId::new(review.clone())),
        "the review session opens"
    );
    let prompt = scripted.body_of(&format!("POST /api/session/{review}/prompt"));
    assert!(
        prompt["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("Please review the current workspace changes"))
    );
    assert!(
        !scripted
            .calls()
            .iter()
            .any(|call| call.contains("/api/auto-review")),
        "a manual review never starts a run"
    );
    close(cx, window, &workspace);
}
