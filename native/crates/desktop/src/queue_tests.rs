//! Headless queue, steer, starter and double-Escape tests against a scripted
//! loopback server that keeps a server-owned queue.

use gpui_kit::Window;
use openchamber_native_core::draft_starters::StarterRef;
use openchamber_native_core::ids::MessageId;
use openchamber_native_core::message_queue::{FollowUpBehavior, QueuedItemId};
use openchamber_native_core::session::LiveStatus;
use openchamber_native_transport::client::ServerSettings;
use openchamber_native_transport::event::ServerEvent;
use serde_json::json;

use super::*;

/// What the scripted server holds and how it answers.
#[derive(Default)]
struct State {
    items: Vec<Value>,
    revision: u64,
    next_item: u64,
    /// Route suffixes (`items`, `take`, `prompt`) answered with a 500.
    fail: Vec<&'static str>,
    /// A path suffix whose answer waits until this is cleared.
    hold: Option<&'static str>,
    /// Project setup reads still to fail.
    project_failures: u32,
    project_save_failures: u32,
    /// The project's starters as `{type, name, source}`.
    project_starters: Vec<Value>,
    reservation: Option<Value>,
    drop_reservation_reply: bool,
    invalidate_after_preview: bool,
}

struct QueueServer {
    endpoint: Endpoint,
    requests: Arc<Mutex<Vec<Request>>>,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

fn session_body(state: &State) -> Value {
    json!({"sessionId": "ses_queue", "directory": "/project", "items": state.items, "sendingId": null})
}

fn answer(request: &Request, state: &mut State) -> (&'static str, String) {
    let path = request
        .path
        .split('?')
        .next()
        .unwrap_or_default()
        .to_owned();
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let last = path.rsplit('/').next().unwrap_or_default().to_owned();
    if state.fail.iter().any(|fail| *fail == last) {
        return ("500 Internal Server Error", "{}".to_owned());
    }
    let changed = |state: &mut State| {
        state.revision += 1;
        json!({"revision": state.revision, "session": session_body(state)})
    };
    if path == "/api/magic-prompts" {
        return ("200 OK", r#"{"version":1,"overrides":{}}"#.to_owned());
    }
    if path.starts_with("/api/projects/") && path.ends_with("/config") {
        if request.method == "GET" && state.project_failures > 0 {
            state.project_failures -= 1;
            return ("500 Internal Server Error", "{}".to_owned());
        }
        if request.method == "PUT" {
            if state.project_save_failures > 0 {
                state.project_save_failures -= 1;
                return ("500 Internal Server Error", "{}".to_owned());
            }
            let personal: Vec<Value> = body["draftStarters"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|mut entry| {
                    entry["source"] = json!("personal");
                    entry
                })
                .collect();
            state
                .project_starters
                .retain(|entry| entry["source"] == "shared");
            state.project_starters.extend(personal);
        }
        return (
            "200 OK",
            json!({"draftStarters": state.project_starters}).to_string(),
        );
    }
    let queue = path.strip_prefix("/api/message-queue/sessions/ses_queue");
    match (request.method.as_str(), queue) {
        ("POST", Some("/reserve-prefix" | "/preview-prefix")) => {
            let preview = queue == Some("/preview-prefix");
            if state.reservation.is_none() {
                let ids = body["itemIds"].as_array().expect("prefix ids");
                if ids.is_empty()
                    || !ids.iter().enumerate().all(|(index, id)| {
                        state.items.get(index).is_some_and(|item| &item["id"] == id)
                    })
                {
                    return (
                        "409 Conflict",
                        json!({"code":"queue_prefix_unavailable",
                        "sessionID":"ses_queue", "messageID":body["id"]})
                        .to_string(),
                    );
                }
                if !preview {
                    state.reservation = Some(body.clone());
                }
            }
            let receipt = state.reservation.as_ref().unwrap_or(&body);
            if receipt != &body {
                return (
                    "409 Conflict",
                    json!({"code":"queue_prefix_unavailable",
                    "sessionID":"ses_queue", "messageID":body["id"]})
                    .to_string(),
                );
            }
            let count = receipt["itemIds"].as_array().expect("ids").len();
            let response = json!({"revision": state.revision, "session": session_body(state), "reservation": receipt, "items": &state.items[..count]}).to_string();
            if preview && std::mem::take(&mut state.invalidate_after_preview) {
                state.items.clear();
                state.revision += 1;
            }
            ("200 OK", response)
        }
        ("POST", Some("/items")) => {
            state.next_item += 1;
            let mut item = body["item"].clone();
            item["id"] = json!(format!("q{}", state.next_item));
            item["createdAt"] = json!(state.next_item);
            state.items.push(item);
            ("200 OK", changed(state).to_string())
        }
        ("POST", Some("/take")) => {
            let items = std::mem::take(&mut state.items);
            let mut response = changed(state);
            response["items"] = json!(items);
            ("200 OK", response.to_string())
        }
        ("POST", Some(rest)) if rest.ends_with("/take") => {
            let id = rest.trim_start_matches("/items/").trim_end_matches("/take");
            let Some(index) = state.items.iter().position(|item| item["id"] == id) else {
                return ("404 Not Found", "{}".to_owned());
            };
            let item = state.items.remove(index);
            let mut response = changed(state);
            response["item"] = item;
            ("200 OK", response.to_string())
        }
        ("DELETE", Some(rest)) => {
            let id = rest.trim_start_matches("/items/");
            state.items.retain(|item| item["id"] != id);
            ("200 OK", changed(state).to_string())
        }
        ("PUT", Some("/order")) => {
            let order: Vec<String> =
                serde_json::from_value(body["itemIds"].clone()).unwrap_or_default();
            state
                .items
                .sort_by_key(|item| order.iter().position(|id| item["id"] == id.as_str()));
            ("200 OK", changed(state).to_string())
        }
        ("GET", _) if path == "/api/message-queue" => (
            "200 OK",
            json!({"revision": state.revision, "sessions": [session_body(state)]}).to_string(),
        ),
        ("PUT", _) if path == "/api/config/settings" => ("200 OK", body.to_string()),
        ("GET", _) if path.ends_with("/inbox") => ("200 OK", r#"{"data":[]}"#.to_owned()),
        ("POST", _) if path.ends_with("/prompt") || path.ends_with("/synthetic") => {
            if path.ends_with("/prompt")
                && state
                    .reservation
                    .as_ref()
                    .is_some_and(|receipt| receipt["id"] == body["id"])
            {
                let receipt = state.reservation.take().expect("receipt");
                let count = receipt["itemIds"].as_array().expect("ids").len();
                state.items.drain(..count);
                state.revision += 1;
            }
            let kind = if path.ends_with("/prompt") {
                "user"
            } else {
                "synthetic"
            };
            (
                "200 OK",
                json!({"data": {"id": body["id"], "sessionID": "ses_queue", "type": kind,
                    "time": {"created": 1}, "delivery": body["delivery"].as_str().unwrap_or("queue"),
                    "payload": {}}})
                .to_string(),
            )
        }
        ("POST", _) if path.ends_with("/interrupt") => {
            ("200 OK", r#"{"interrupted":true}"#.to_owned())
        }
        ("POST", _) => ("204 No Content", String::new()),
        _ => ("404 Not Found", "{}".to_owned()),
    }
}

impl QueueServer {
    fn new(journal: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener binds");
        listener.set_nonblocking(true).expect("nonblocking");
        let endpoint = Endpoint::parse(&format!(
            "http://{}",
            listener.local_addr().expect("address")
        ))
        .expect("endpoint");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new(State::default()));
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
                let mut request = read_request(&mut stream);
                request.journal = std::fs::read(&journal)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok());
                let (status, body) = answer(&request, &mut scripted.lock().expect("state"));
                let path = request
                    .path
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                recorded.lock().expect("requests").push(request);
                if path.ends_with("/reserve-prefix") {
                    let mut state = scripted.lock().expect("state");
                    if std::mem::take(&mut state.drop_reservation_reply) {
                        continue;
                    }
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                while scripted
                    .lock()
                    .expect("state")
                    .hold
                    .is_some_and(|suffix| path.ends_with(suffix))
                    && !stopped.load(Ordering::Acquire)
                {
                    assert!(
                        Instant::now() < deadline,
                        "the test releases the held answer"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
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

    /// `METHOD path` of every request, in order.
    fn calls(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("requests")
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect()
    }

    fn body_of(&self, suffix: &str) -> Value {
        let requests = self.requests.lock().expect("requests");
        let request = requests
            .iter()
            .rfind(|request| request.path.ends_with(suffix))
            .unwrap_or_else(|| panic!("a request to {suffix}"));
        serde_json::from_slice(&request.body).expect("JSON body")
    }
}

impl Drop for QueueServer {
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

struct Harness {
    server: QueueServer,
    window: AnyWindowHandle,
    workspace: Entity<Workspace>,
    key: SessionKey,
    _fixture: Fixture,
}

fn harness(cx: &mut TestAppContext, follow_up: FollowUpBehavior) -> Harness {
    let fixture = Fixture::new();
    let key = tests::key("queue-runtime", "ses_queue");
    let server = QueueServer::new(fixture.journal());
    let (window, workspace) = open(cx, &server.server(), &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx);
            workspace.send_catalog.settings = Some(ServerSettings {
                follow_up_behavior: Some(follow_up),
                ..ServerSettings::default()
            });
            workspace
                .live_status
                .insert(key.session.clone(), LiveStatus::Busy);
        })
    })
    .expect("window");
    Harness {
        server,
        window,
        workspace,
        key,
        _fixture: fixture,
    }
}

impl Harness {
    fn act(
        &self,
        cx: &mut TestAppContext,
        action: impl FnOnce(&mut Workspace, &mut Window, &mut gpui_kit::Context<Workspace>),
    ) {
        cx.update_window(self.window, |_, window, cx| {
            self.workspace
                .update(cx, |workspace, cx| action(workspace, window, cx))
        })
        .expect("window");
    }

    fn type_and_enter(&self, cx: &mut TestAppContext, text: &str, secondary: bool) {
        self.act(cx, |workspace, window, cx| {
            workspace
                .composer
                .update(cx, |composer, cx| composer.set_value(text, window, cx));
            workspace.submit_from_enter(secondary, window, cx);
        });
    }

    fn composer(&self, cx: &mut TestAppContext) -> String {
        cx.read(|cx| {
            self.workspace
                .read(cx)
                .composer
                .read(cx)
                .value()
                .to_string()
        })
    }

    fn queued(&self, cx: &mut TestAppContext) -> Vec<String> {
        cx.read(|cx| {
            self.workspace
                .read(cx)
                .selected_queue()
                .iter()
                .map(|message| message.content.clone())
                .collect()
        })
    }

    fn idle(&self, cx: &mut TestAppContext) {
        wait(cx, |cx| {
            cx.read(|cx| !self.workspace.read(cx).send_attempts.is_active(&self.key))
        });
    }
}

#[gpui_kit::test]
fn enter_queues_while_busy_with_the_captured_selection(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "\nfollow up\n", false);
    assert_eq!(
        harness.composer(cx),
        "",
        "queueing clears the composer at once"
    );
    wait(cx, |cx| harness.queued(cx) == ["follow up"]);
    let body = harness.server.body_of("/items");
    assert_eq!(body["directory"], "/project");
    assert_eq!(
        body["item"]["content"], "follow up",
        "edge newlines are trimmed"
    );
    assert_eq!(
        body["item"]["sendConfig"],
        json!({"providerID": "provider", "modelID": "model", "agent": "agent", "variant": "effort"})
    );
    assert!(
        !harness
            .server
            .calls()
            .iter()
            .any(|call| call.ends_with("/prompt")),
        "nothing is sent while the turn runs"
    );
}

#[gpui_kit::test]
fn a_failed_queue_gives_the_text_back_after_newer_typing(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness
        .server
        .state
        .lock()
        .expect("state")
        .fail
        .push("items");
    harness.type_and_enter(cx, "lost?", false);
    harness.act(cx, |workspace, window, cx| {
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("newer", window, cx));
    });
    wait(cx, |cx| harness.composer(cx) == "newer\n\nlost?");
    assert!(harness.queued(cx).is_empty());
}

#[gpui_kit::test]
fn steer_mode_steers_and_ctrl_enter_sends_the_queue_now(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Steer);
    harness.type_and_enter(cx, "steer this", false);
    harness.idle(cx);
    assert_eq!(harness.server.body_of("/prompt")["delivery"], "steer");
    assert!(harness.queued(cx).is_empty());

    // Two queued by another device, then Ctrl+Enter.
    harness.act(cx, |workspace, _, cx| {
        workspace.queue_composer_text_for_test("one", cx)
    });
    wait(cx, |cx| harness.queued(cx).len() == 1);
    harness.act(cx, |workspace, _, cx| {
        workspace.queue_composer_text_for_test("two", cx)
    });
    wait(cx, |cx| harness.queued(cx).len() == 2);
    harness.type_and_enter(cx, "three", true);
    wait(cx, |cx| {
        harness.queued(cx).is_empty() && harness.composer(cx).is_empty()
    });
    harness.idle(cx);
    let prompt = harness.server.body_of("/prompt");
    assert_eq!(
        prompt["text"], "one\n\ntwo\n\nthree",
        "queued first, then the composer"
    );
    assert!(
        prompt.get("delivery").is_none(),
        "Ctrl+Enter leaves delivery to OpenCode"
    );
    assert!(
        harness
            .server
            .calls()
            .iter()
            .any(|call| { call == "POST /api/message-queue/sessions/ses_queue/reserve-prefix" })
    );
}

#[gpui_kit::test]
fn row_send_steers_with_the_captured_selection_and_arrow_up_edits(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "first", false);
    harness.type_and_enter(cx, "second", false);
    wait(cx, |cx| harness.queued(cx).len() == 2);
    let ids: Vec<QueuedItemId> = cx.read(|cx| {
        harness
            .workspace
            .read(cx)
            .selected_queue()
            .iter()
            .map(|message| message.id.clone())
            .collect()
    });
    // Reorder: second before first.
    harness.act(cx, |workspace, _, cx| {
        workspace.reorder_queued(&ids[1], &ids[0], cx)
    });
    assert_eq!(
        harness.queued(cx),
        ["second", "first"],
        "the order shows at once"
    );
    wait(cx, |_| {
        harness
            .server
            .calls()
            .iter()
            .any(|call| call.ends_with("/order"))
    });
    assert_eq!(
        harness.server.body_of("/order")["itemIds"],
        json!(["q2", "q1"])
    );

    harness.act(cx, |workspace, window, cx| {
        workspace.send_queued(&ids[1], window, cx)
    });
    wait(cx, |cx| harness.queued(cx) == ["first"]);
    harness.idle(cx);
    let prompt = harness.server.body_of("/prompt");
    assert_eq!(prompt["text"], "second");
    assert_eq!(
        prompt["delivery"], "steer",
        "a row's send steers while busy"
    );
    assert_eq!(
        harness.server.body_of("/model")["model"]["variant"],
        "effort"
    );

    // ArrowUp in the empty composer takes the newest message back.
    let newest = cx.read(|cx| harness.workspace.read(cx).newest_editable_queued());
    assert_eq!(newest, Some(ids[0].clone()));
    harness.act(cx, |workspace, window, cx| {
        workspace.edit_queued(&ids[0], window, cx)
    });
    wait(cx, |cx| harness.composer(cx) == "first");
    assert!(
        harness.queued(cx).is_empty(),
        "editing takes it out of the queue"
    );

    harness.type_and_enter(cx, "gone", false);
    wait(cx, |cx| harness.queued(cx) == ["gone"]);
    let id = cx.read(|cx| harness.workspace.read(cx).selected_queue()[0].id.clone());
    harness.act(cx, |workspace, _, cx| workspace.remove_queued(&id, cx));
    assert!(harness.queued(cx).is_empty(), "removal shows at once");
    wait(cx, |_| {
        harness
            .server
            .calls()
            .iter()
            .any(|call| call.starts_with("DELETE"))
    });
}

#[gpui_kit::test]
fn broadcasts_apply_by_revision_and_deletion_forgets_the_queue(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    let update = |revision, contents: &[&str]| {
        let items: Vec<Value> = contents
            .iter()
            .enumerate()
            .map(|(index, content)| json!({"id": format!("b{index}"), "createdAt": 1, "content": content,
                "text": content, "attachments": [], "sendConfig": {"providerID": "p", "modelID": "m"}}))
            .collect();
        let properties = json!({"revision": revision, "session": {"sessionId": "ses_queue",
            "directory": "/project", "items": items, "sendingId": null}});
        let frame = json!({"type": "event", "payload": {"type": "openchamber:message-queue.updated", "properties": properties}});
        openchamber_native_transport::event::decode_frame(&frame.to_string())
            .expect("valid frame")
            .events
    };
    harness.act(cx, |workspace, window, cx| {
        workspace.apply_server_events(update(5, &["new"]), window, cx);
        workspace.apply_server_events(update(4, &["stale"]), window, cx);
    });
    assert_eq!(harness.queued(cx), ["new"]);
    harness.act(cx, |workspace, window, cx| {
        workspace.apply_server_events(
            vec![ServerEvent::SessionDeleted {
                session_id: harness.key.session.clone(),
            }],
            window,
            cx,
        );
        let runtime = workspace.runtime.clone();
        let key = openchamber_native_core::message_queue::QueueKey::new(
            runtime,
            "/project",
            harness.key.session.clone(),
        )
        .expect("valid key");
        assert!(
            workspace.queue.projection.queue(&key).is_empty(),
            "deletion forgets the queue"
        );
    });
}

#[gpui_kit::test]
fn escape_stops_only_on_a_second_press_within_three_seconds(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    let interrupts = |server: &QueueServer| {
        server
            .calls()
            .iter()
            .filter(|call| call.ends_with("/interrupt"))
            .count()
    };
    harness.act(cx, |workspace, window, cx| {
        workspace.focus_composer(window, cx);
    });
    cx.run_until_parked();
    harness.act(cx, |workspace, window, cx| {
        workspace.escape_to_stop(window, cx)
    });
    cx.run_until_parked();
    assert_eq!(interrupts(&harness.server), 0, "one Escape only arms");
    // An expired arm re-arms instead of stopping.
    harness.act(cx, |workspace, window, cx| {
        if let Some((_, until)) = workspace.abort_primed.as_mut() {
            *until = Instant::now() - Duration::from_millis(1);
        }
        workspace.escape_to_stop(window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        interrupts(&harness.server),
        0,
        "a press after three seconds arms again"
    );
    harness.act(cx, |workspace, window, cx| {
        workspace.escape_to_stop(window, cx)
    });
    wait(cx, |_| interrupts(&harness.server) == 1);
    // Escape away from the composer never arms.
    harness.act(cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle.clone(), cx);
    });
    cx.run_until_parked();
    harness.act(cx, |workspace, window, cx| {
        workspace.escape_to_stop(window, cx);
        assert!(workspace.abort_primed.is_none());
    });
}

#[gpui_kit::test]
fn starter_edits_save_the_global_list_with_both_markers(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.act(cx, |workspace, _, cx| {
        workspace.remove_starter("global:command:explore", cx);
    });
    wait(cx, |_| {
        harness
            .server
            .calls()
            .iter()
            .any(|call| call.starts_with("PUT /api/config/settings"))
    });
    let body = harness
        .server
        .body_of("/api/config/settings?surface=desktop");
    let names: Vec<&str> = body["draftStarters"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|starter| starter["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(names.len(), 7);
    assert!(!names.contains(&"explore"));
    assert_eq!(body["draftStartersCraftGoalAdded"], true);
    assert_eq!(body["draftStartersScheduleTaskAdded"], true);
    harness.act(cx, |workspace, _, cx| {
        assert!(
            workspace
                .pinnable_starters()
                .contains(&StarterRef::command("explore"))
        );
        workspace.add_starter(StarterRef::command("explore"), cx);
        assert!(workspace.pinnable_starters().is_empty());
    });
}

impl Workspace {
    /// Queues `text` as another device would, without touching the composer.
    fn queue_composer_text_for_test(&mut self, text: &str, cx: &mut gpui_kit::Context<Self>) {
        let key = self.selected_queue_key().expect("a selected queue");
        let connection = self.connection.as_ref().expect("connected");
        let client = connection.client.clone().expect("client");
        let item = openchamber_native_transport::message_queue::QueueItemInput {
            content: text.to_owned(),
            text: text.to_owned(),
            send_config: openchamber_native_core::message_queue::QueuedSendConfig {
                provider_id: "provider".to_owned(),
                model_id: "model".to_owned(),
                agent: None,
                variant: None,
            },
        };
        let request = connection.network.run(async move {
            client
                .enqueue_message(&key.session, &key.directory, &item)
                .await
        });
        cx.spawn(async move |this, cx| {
            if let Some(Ok(update)) = request.await {
                let _ = this.update(cx, |this, cx| {
                    this.apply_queue_broadcast(update, None);
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

/// A broadcast queue of `items` at `revision`, as another device leaves it.
fn broadcast(revision: u64, items: Vec<Value>) -> Vec<ServerEvent> {
    let properties = json!({"revision": revision, "session": {"sessionId": "ses_queue",
        "directory": "/project", "items": items, "sendingId": null}});
    let frame = json!({"type": "event", "payload": {"type": "openchamber:message-queue.updated", "properties": properties}});
    openchamber_native_transport::event::decode_frame(&frame.to_string())
        .expect("valid frame")
        .events
}

fn item(id: &str, extra: Value) -> Value {
    let mut item = json!({"id": id, "createdAt": 1, "content": id, "text": id, "attachments": [],
        "sendConfig": {"providerID": "provider", "modelID": "model"}});
    for (key, value) in extra.as_object().expect("an object") {
        item[key] = value.clone();
    }
    item
}

fn count(server: &QueueServer, needle: &str) -> usize {
    server
        .calls()
        .iter()
        .filter(|call| call.contains(needle))
        .count()
}

fn branching_command_leaves_the_queue(
    text: &str,
    secondary: bool,
    cx: &mut TestAppContext,
) -> String {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.act(cx, |workspace, window, cx| {
        workspace.apply_server_events(broadcast(1, vec![item("q1", json!({}))]), window, cx)
    });
    harness.type_and_enter(cx, text, secondary);
    cx.run_until_parked();
    assert_eq!(
        count(&harness.server, "/message-queue/sessions/ses_queue/items"),
        0,
        "nothing was queued"
    );
    assert_eq!(
        count(&harness.server, "/take"),
        0,
        "a branching command never takes the queue"
    );
    assert_eq!(harness.queued(cx), ["q1"]);
    harness.composer(cx)
}

#[gpui_kit::test]
fn fork_on_enter_is_never_queued(cx: &mut TestAppContext) {
    let left = branching_command_leaves_the_queue("/fork", false, cx);
    assert_eq!(left, "", "the fork took the text");
}

#[gpui_kit::test]
fn host_switch_keeps_a_committed_queue_take_alive_until_its_text_is_restored(
    cx: &mut TestAppContext,
) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "retained from old host", false);
    wait(cx, |cx| harness.queued(cx).len() == 1);
    let owner = cx
        .read(|cx| harness.workspace.read(cx).composer_key.clone())
        .expect("draft owner");
    let id = cx.read(|cx| harness.workspace.read(cx).selected_queue()[0].id.clone());
    harness.server.state.lock().expect("state").hold = Some("/take");
    harness.act(cx, |workspace, window, cx| {
        workspace.edit_queued(&id, window, cx)
    });
    wait(cx, |_| {
        harness.server.state.lock().expect("state").items.is_empty()
    });
    harness.act(cx, |workspace, window, cx| {
        workspace.connect_to_host(
            Endpoint::parse("http://127.0.0.1:1/").expect("endpoint"),
            window,
            cx,
        );
        workspace.switch_runtime(
            openchamber_native_core::ids::RuntimeId::from("new-host"),
            window,
            cx,
        );
        workspace.release_retired_networks();
        assert_eq!(
            workspace.retired_networks.len(),
            1,
            "the committed take still needs its response"
        );
    });
    harness.server.state.lock().expect("state").hold = None;
    wait(cx, |cx| {
        cx.read(|cx| {
            harness
                .workspace
                .read(cx)
                .drafts
                .get(&owner)
                .is_some_and(|draft| draft.text == "retained from old host")
        })
    });
    assert_ne!(
        harness.composer(cx),
        "retained from old host",
        "old-host content stays out of the new host's composer"
    );
}

#[gpui_kit::test]
fn btw_on_ctrl_enter_never_takes_the_queue(cx: &mut TestAppContext) {
    // The scripted server cannot fork, so the side conversation's own
    // failure path may give the question back; it is never queued.
    let _ = branching_command_leaves_the_queue("/btw what now", true, cx);
}

#[gpui_kit::test]
fn steer_mode_enter_takes_the_queue_first(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Steer);
    harness.act(cx, |workspace, _, cx| {
        workspace.queue_composer_text_for_test("one", cx)
    });
    wait(cx, |cx| harness.queued(cx).len() == 1);
    harness.type_and_enter(cx, "two", false);
    wait(cx, |cx| harness.queued(cx).is_empty());
    harness.idle(cx);
    let prompt = harness.server.body_of("/prompt");
    assert_eq!(prompt["text"], "one\n\ntwo");
    assert_eq!(prompt["delivery"], "steer");
}

#[gpui_kit::test]
fn open_requests_neither_block_queueing_nor_get_dismissed_by_a_row_send(cx: &mut TestAppContext) {
    use openchamber_native_core::pending_requests::PendingRequest;
    use openchamber_native_core::permission::PermissionRequest;
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.act(cx, |workspace, window, cx| {
        super::request_tests::ask(
            workspace,
            PendingRequest::Permission(PermissionRequest {
                id: "per_q".into(),
                session_id: "ses_queue".into(),
                action: "shell".into(),
                resources: vec!["printf".into()],
                save: Vec::new(),
                source: None,
                message: None,
                metadata_display: None,
            }),
            window,
            cx,
        );
    });
    harness.type_and_enter(cx, "later", false);
    wait(cx, |cx| harness.queued(cx) == ["later"]);
    let id = cx.read(|cx| harness.workspace.read(cx).selected_queue()[0].id.clone());
    harness.act(cx, |workspace, window, cx| {
        workspace.send_queued(&id, window, cx)
    });
    wait(cx, |_| count(&harness.server, "/prompt") == 1);
    harness.idle(cx);
    assert_eq!(
        count(&harness.server, "/reply"),
        0,
        "no request was answered"
    );
    assert_eq!(harness.server.body_of("/prompt")["text"], "later");
}

#[gpui_kit::test]
fn a_lost_reservation_reply_recovers_the_same_message_without_losing_new_typing(
    cx: &mut TestAppContext,
) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "A", false);
    wait(cx, |cx| harness.queued(cx).len() == 1);
    harness.type_and_enter(cx, "B", false);
    wait(cx, |cx| harness.queued(cx).len() == 2);
    harness
        .server
        .state
        .lock()
        .expect("state")
        .drop_reservation_reply = true;
    harness.type_and_enter(cx, "typed", true);
    wait(cx, |cx| {
        cx.read(|cx| {
            harness
                .workspace
                .read(cx)
                .recovery_items(cx)
                .iter()
                .any(|row| {
                    row.key == harness.key
                        && row.phase == crate::recovery_view::RecoveryPhase::NotObserved
                })
        })
    });
    let id = harness
        .server
        .state
        .lock()
        .expect("state")
        .reservation
        .as_ref()
        .expect("server retained receipt")["id"]
        .clone();
    {
        let requests = harness.server.requests.lock().expect("recorded requests");
        let reservation = requests
            .iter()
            .find(|request| request.path.ends_with("/reserve-prefix"))
            .expect("reservation request reached the server");
        let stored = reservation
            .journal
            .as_ref()
            .expect("journal exists before reservation");
        assert_eq!(stored["attempts"][0]["prompt"]["message_id"], id);
        assert_eq!(stored["attempts"][0]["reservation"]["composer"], "typed");
        assert_eq!(stored["attempts"][0]["prompt"]["text"], "A\n\nB\n\ntyped");
    }
    assert_eq!(count(&harness.server, "/prompt"), 0);
    assert_eq!(harness.composer(cx), "");
    assert_eq!(harness.server.state.lock().expect("state").items.len(), 2);
    let queued_before = count(
        &harness.server,
        "POST /api/message-queue/sessions/ses_queue/items",
    );
    harness.type_and_enter(cx, "newer", false);
    assert_eq!(
        count(
            &harness.server,
            "POST /api/message-queue/sessions/ses_queue/items"
        ),
        queued_before,
        "an uncertain reservation cannot queue the captured text a second time"
    );
    close(cx, harness.window, &harness.workspace);
    let (reopened_window, reopened) = open(
        cx,
        &harness.server.server(),
        &harness._fixture,
        &harness.key,
    );
    ready(cx, &reopened);
    cx.update_window(reopened_window, |_, window, cx| {
        reopened.update(cx, |workspace, cx| {
            configure_send(workspace, &harness.key, window, cx);
            assert_eq!(workspace.composer.read(cx).value().as_ref(), "newer");
            assert!(!workspace.send_attempts.can_send(&harness.key));
            workspace.retry_recovery(
                &harness.key,
                &MessageId::new(id.as_str().expect("message ID")),
                window,
                cx,
            );
        });
    })
    .expect("reopened window");
    wait(cx, |_| count(&harness.server, "/prompt") == 1);
    wait(cx, |cx| {
        cx.read(|cx| !reopened.read(cx).send_attempts.is_active(&harness.key))
    });
    let prompt = harness.server.body_of("/prompt");
    assert_eq!(prompt["id"], id);
    assert_eq!(prompt["text"], "A\n\nB\n\ntyped");
    assert_eq!(
        cx.read(|cx| reopened.read(cx).composer.read(cx).value().to_string()),
        "newer"
    );
    assert_eq!(
        count(&harness.server, "/take"),
        0,
        "no destructive take is used"
    );
    close(cx, reopened_window, &reopened);
}

#[gpui_kit::test]
fn an_empty_take_without_text_sends_nothing(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.act(cx, |workspace, window, cx| {
        workspace.live_status.remove(&harness.key.session);
        // Shown here, already delivered on the server.
        workspace.apply_server_events(broadcast(1, vec![item("gone", json!({}))]), window, cx)
    });
    harness.type_and_enter(cx, "", false);
    wait(cx, |_| count(&harness.server, "/preview-prefix") == 1);
    cx.run_until_parked();
    assert_eq!(count(&harness.server, "/prompt"), 0);
    assert!(
        cx.read(|cx| harness.workspace.read(cx).persistence_ready()),
        "the journal never saw an empty prompt"
    );
}

#[gpui_kit::test]
fn a_stale_preview_restores_only_the_captured_composer_and_releases_the_session(
    cx: &mut TestAppContext,
) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "already delivered", false);
    wait(cx, |cx| harness.queued(cx).len() == 1);
    harness
        .server
        .state
        .lock()
        .expect("queue state")
        .invalidate_after_preview = true;
    harness.type_and_enter(cx, "still unsent", true);
    wait(cx, |cx| {
        count(&harness.server, "/reserve-prefix") == 1
            && cx.read(|cx| {
                harness
                    .workspace
                    .read(cx)
                    .send_attempts
                    .can_send(&harness.key)
            })
    });
    assert_eq!(harness.composer(cx), "still unsent");
    assert_eq!(count(&harness.server, "/prompt"), 0);
    assert!(
        harness
            .server
            .state
            .lock()
            .expect("queue state")
            .reservation
            .is_none()
    );
    harness.type_and_enter(cx, "still unsent", false);
    wait(cx, |cx| harness.queued(cx) == ["still unsent"]);
}

#[gpui_kit::test]
fn retry_of_a_row_send_keeps_open_requests_and_its_prompt_identity(cx: &mut TestAppContext) {
    use openchamber_native_core::pending_requests::PendingRequest;
    use openchamber_native_core::permission::PermissionRequest;
    let harness = harness(cx, FollowUpBehavior::Queue);
    harness.type_and_enter(cx, "queued", false);
    wait(cx, |cx| harness.queued(cx).len() == 1);
    harness
        .server
        .state
        .lock()
        .expect("state")
        .fail
        .push("prompt");
    harness.act(cx, |workspace, window, cx| {
        super::request_tests::ask(
            workspace,
            PendingRequest::Permission(PermissionRequest {
                id: "per_retry".into(),
                session_id: "ses_queue".into(),
                action: "shell".into(),
                resources: vec!["printf".into()],
                save: Vec::new(),
                source: None,
                message: None,
                metadata_display: None,
            }),
            window,
            cx,
        );
        let id = workspace.selected_queue()[0].id.clone();
        workspace.send_queued(&id, window, cx);
    });
    wait(cx, |_| count(&harness.server, "/prompt") == 1);
    harness.idle(cx);
    let original = harness.server.body_of("/prompt");
    let id = MessageId::new(original["id"].as_str().expect("message id"));
    assert!(cx.read(|cx| {
        !harness
            .workspace
            .read(cx)
            .send_attempts
            .attempt(&harness.key, &id)
            .expect("retained send")
            .supersedes
    }));
    harness.server.state.lock().expect("state").fail.clear();
    // The UI Retry path performs an exact-ID observation before dispatch.
    wait(cx, |cx| {
        cx.read(|cx| {
            harness
                .workspace
                .read(cx)
                .recovery_items(cx)
                .iter()
                .any(|row| row.message == id && row.can_retry)
        })
    });
    harness.act(cx, |workspace, window, cx| {
        workspace.retry_recovery(&harness.key, &id, window, cx)
    });
    wait(cx, |_| count(&harness.server, "/prompt") == 2);
    harness.idle(cx);
    assert_eq!(harness.server.body_of("/prompt")["id"], original["id"]);
    assert_eq!(
        count(&harness.server, "/reply"),
        0,
        "neither the original send nor Retry dismisses requests"
    );
}

#[gpui_kit::test]
fn goal_and_unjournalable_messages_stay_for_the_server(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    let big = json!({"attachments": [{"id": "a", "filename": "big.png", "mimeType": "image/png",
        "size": 100_000, "source": "local"}]});
    harness.act(cx, |workspace, window, cx| {
        workspace.live_status.remove(&harness.key.session);
        workspace.apply_server_events(
            broadcast(
                1,
                vec![
                    item("goal", json!({"goal": {"tokenBudget": null}})),
                    item("big", big),
                ],
            ),
            window,
            cx,
        );
        for id in ["goal", "big"] {
            workspace.send_queued(&QueuedItemId::new(id), window, cx);
        }
    });
    harness.type_and_enter(cx, "", false);
    cx.run_until_parked();
    assert_eq!(
        count(&harness.server, "/take"),
        0,
        "neither message was taken"
    );
    assert_eq!(harness.queued(cx), ["goal", "big"]);
    assert_eq!(
        cx.read(|cx| harness.workspace.read(cx).newest_editable_queued()),
        None
    );
}

/// A new-session draft in the registered `/project`.
fn draft_in_project(harness: &Harness, cx: &mut TestAppContext) {
    harness.act(cx, |workspace, window, cx| {
        workspace.topology.projects = vec![openchamber_native_core::ownership::ConfiguredProject {
            id: openchamber_native_core::ids::ProjectId::from("registered"),
            path: "/project".into(),
        }];
        workspace.show_new_session(window, cx);
        workspace.choose_draft_project("/project", window, cx);
        assert!(workspace.can_create_connected());
    });
}

#[gpui_kit::test]
fn a_starter_takes_only_the_text_it_carries_and_gives_it_back_on_failure(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    draft_in_project(&harness, cx);
    let chip = "global:command:craft-goal";
    harness.server.state.lock().expect("state").hold = Some("/magic-prompts");
    harness.act(cx, |workspace, window, cx| {
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("idea", window, cx));
        workspace.activate_starter(chip, window, cx);
        workspace.save_composer_draft(cx);
        assert_eq!(
            workspace
                .drafts
                .get(workspace.composer_key.as_ref().expect("owner"))
                .expect("captured draft remains")
                .text,
            "idea"
        );
        assert_eq!(
            workspace.composer.read(cx).value().as_ref(),
            "",
            "the carried text leaves at once"
        );
        workspace.composer.update(cx, |composer, cx| {
            composer.set_value("typed while loading", window, cx)
        });
        workspace.save_composer_draft(cx);
        assert_eq!(
            workspace
                .drafts
                .get(workspace.composer_key.as_ref().expect("owner"))
                .expect("both texts remain")
                .text,
            "typed while loading\n\nidea"
        );
        let _ = workspace.persistence_barrier(cx);
    });
    wait(cx, |_| {
        fs::read(harness._fixture.journal())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|stored| {
                stored["drafts"].as_array().is_some_and(|drafts| {
                    drafts
                        .iter()
                        .any(|draft| draft["text"] == "typed while loading\n\nidea")
                })
            })
    });
    harness.server.state.lock().expect("state").hold = None;
    wait(cx, |cx| {
        cx.read(|cx| harness.workspace.read(cx).creations.iter().count() == 1)
    });
    assert_eq!(
        harness.composer(cx),
        "typed while loading",
        "typing during the read stays"
    );
    let (draft, prompt) = cx.read(|cx| {
        let workspace = harness.workspace.read(cx);
        let creation = workspace.creations.iter().next().expect("a creation").1;
        (creation.draft.text.clone(), creation.prompt.text.clone())
    });
    assert_eq!(
        draft, "/craft-goal idea",
        "a failure gives back what the chip submitted"
    );
    assert!(
        prompt.ends_with("Here is my initial idea:\nidea"),
        "the session gets the render"
    );
    assert!(cx.read(|cx| harness.workspace.read(cx).persistence_ready()));
}

#[gpui_kit::test]
fn a_failed_starter_read_gives_the_carried_text_back_after_newer_typing(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    draft_in_project(&harness, cx);
    let chip = "global:command:craft-goal";
    harness
        .server
        .state
        .lock()
        .expect("state")
        .fail
        .push("magic-prompts");
    harness.act(cx, |workspace, window, cx| {
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("second", window, cx));
        workspace.activate_starter(chip, window, cx);
        workspace
            .composer
            .update(cx, |composer, cx| composer.set_value("newer", window, cx));
    });
    wait(cx, |cx| harness.composer(cx) == "newer\n\nsecond");
}

#[gpui_kit::test]
fn project_starters_retry_a_failed_read_and_keep_edits_made_during_a_save(cx: &mut TestAppContext) {
    let harness = harness(cx, FollowUpBehavior::Queue);
    {
        let mut state = harness.server.state.lock().expect("state");
        // The Project section reads the same setup route.
        state.project_failures = 2;
        state.project_starters = vec![
            json!({"type": "command", "name": "explore", "source": "personal"}),
            json!({"type": "command", "name": "debug", "source": "personal"}),
        ];
    }
    draft_in_project(&harness, cx);
    let reads = |server: &QueueServer| {
        server
            .calls()
            .iter()
            .filter(|call| call.starts_with("GET /api/projects/"))
            .count()
    };
    harness.act(cx, |workspace, _, cx| workspace.ensure_project_starters(cx));
    wait(cx, |_| reads(&harness.server) >= 2);
    cx.run_until_parked();
    let failed = reads(&harness.server);
    harness.act(cx, |workspace, _, cx| workspace.ensure_project_starters(cx));
    cx.run_until_parked();
    assert_eq!(
        reads(&harness.server),
        failed,
        "a failed read waits before it runs again"
    );
    thread::sleep(READ_RETRY_WAIT);
    harness.act(cx, |workspace, _, cx| workspace.ensure_project_starters(cx));
    let project = |cx: &mut TestAppContext| {
        cx.read(|cx| {
            harness
                .workspace
                .read(cx)
                .resolved_starters()
                .into_iter()
                .filter(|chip| chip.id.starts_with("project:"))
                .map(|chip| chip.starter.name)
                .collect::<Vec<_>>()
        })
    };
    wait(cx, |cx| project(cx) == ["explore", "debug"]);

    harness.server.state.lock().expect("state").hold = Some("/config");
    harness
        .server
        .state
        .lock()
        .expect("state")
        .project_save_failures = 1;
    harness.act(cx, |workspace, _, cx| {
        workspace.remove_starter("project:command:explore", cx);
        workspace.remove_starter("project:command:debug", cx);
    });
    assert!(
        project(cx).is_empty(),
        "both removals show while the first save is out"
    );
    harness.server.state.lock().expect("state").hold = None;
    wait(cx, |_| {
        harness
            .server
            .calls()
            .iter()
            .filter(|call| call.starts_with("PUT /api/projects/"))
            .count()
            == 2
    });
    assert_eq!(
        harness.server.body_of("/config")["draftStarters"],
        json!([])
    );
    wait(cx, |cx| project(cx).is_empty());
}

/// Longer than the first retry wait of a failed project read.
const READ_RETRY_WAIT: Duration = Duration::from_millis(2100);
