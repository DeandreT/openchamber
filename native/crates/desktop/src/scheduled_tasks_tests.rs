//! Scheduled tasks against a scripted loopback server that answers the
//! scheduled task routes and records every request. The harness is shared
//! with the Project knowledge tests.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext, ElementId, Entity, SharedString, TestAppContext};
use openchamber_native_core::schedule::ScheduleKind;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use serde_json::{Value, json};

use crate::network::Network;
use crate::scheduled_tasks::{ScheduledTasks, Scope, TaskList};

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub body: Value,
}

type Answer = Box<dyn FnMut(&Request) -> (u16, Value) + Send>;

/// A loopback server answering by `answer`; dropping it stops the worker.
pub(crate) struct Scripted {
    pub endpoint: Endpoint,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Scripted {
    pub(crate) fn new(answer: impl FnMut(&Request) -> (u16, Value) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener binds");
        listener.set_nonblocking(true).expect("nonblocking");
        let endpoint = Endpoint::parse(&format!(
            "http://{}",
            listener.local_addr().expect("address")
        ))
        .expect("endpoint");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, stopped) = (requests.clone(), stop.clone());
        let mut answer: Answer = Box::new(answer);
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
                stream.set_nonblocking(false).expect("blocking stream");
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                log.lock().expect("log").push(request.clone());
                let (status, body) = answer(&request);
                let body = body.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Scripted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            endpoint,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    pub(crate) fn client(&self) -> ServerClient {
        ServerClient::new(self.endpoint.clone(), Credential::None).expect("client")
    }

    pub(crate) fn requests(&self, method: &str, path: &str) -> Vec<Request> {
        self.requests
            .lock()
            .expect("log")
            .iter()
            .filter(|request| request.method == method && request.path == path)
            .cloned()
            .collect()
    }
}

impl Drop for Scripted {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("read timeout");
    let mut bytes = Vec::new();
    let end = loop {
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or_default();
    while bytes.len() < end + length {
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    let mut line = head.lines().next()?.split(' ');
    let method = line.next()?.to_owned();
    let target = line.next()?;
    Some(Request {
        method,
        path: target.split('?').next()?.to_owned(),
        body: serde_json::from_slice(&bytes[end..end + length]).unwrap_or(Value::Null),
    })
}

#[track_caller]
pub(crate) fn wait(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
    let started = Instant::now();
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "background work completes within ten seconds"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

pub(crate) fn name(id: impl Into<String>) -> ElementId {
    ElementId::Name(SharedString::from(id.into()))
}

pub(crate) fn painted(cx: &mut TestAppContext, window: AnyWindowHandle, id: &str) -> bool {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.try_find(name(id)).is_some()
    })
    .expect("window is open")
}

const PROJECT: &str = "proj-1";
const LIST: &str = "/api/projects/proj-1/scheduled-tasks";

fn stored_task(enabled: bool) -> Value {
    json!({
        "id": "t1", "name": "Nightly digest", "enabled": enabled, "futureField": "kept",
        "schedule": {"kind": "daily", "times": ["09:00"], "timezone": "UTC"},
        "execution": {"prompt": "Summarize", "providerID": "p", "modelID": "m"},
        "state": {"createdAt": 1, "updatedAt": 2, "lastStatus": "error", "lastError": "Model unavailable", "lastRunAt": 5}
    })
}

/// The page in the workspace's window, its scopes Chats and one project.
fn open_page(
    cx: &mut TestAppContext,
    server: &Scripted,
    network: &Network,
) -> (AnyWindowHandle, Entity<ScheduledTasks>) {
    cx.executor().allow_parking();
    let (window, workspace) = crate::workspace_tests::open(cx);
    let client = server.client();
    let handle = network.handle();
    let page = cx
        .update_window(window, |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                let owner = cx.entity().downgrade();
                let runtime = workspace.runtime.clone();
                let page =
                    cx.new(|_| ScheduledTasks::new(owner, runtime).with_network(client, handle));
                page.update(cx, |page, cx| {
                    page.open(
                        vec![
                            Scope {
                                id: "openchamber:chats".into(),
                                label: "Chats".into(),
                                path: None,
                            },
                            Scope {
                                id: PROJECT.into(),
                                label: "project".into(),
                                path: Some("/work/project".into()),
                            },
                        ],
                        None,
                        cx,
                    )
                });
                workspace.scheduled_tasks = Some(page.clone());
                workspace.scheduled_tasks_open = true;
                page
            })
        })
        .expect("window is open");
    (window, page)
}

fn toast(cx: &mut TestAppContext, page: &Entity<ScheduledTasks>) -> Option<String> {
    cx.read(|cx| page.read(cx).toast.as_ref().map(|toast| toast.text.to_string()))
}

#[gpui_kit::test]
fn the_sidebar_button_opens_the_page_in_place_of_the_board(cx: &mut TestAppContext) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(name("scheduled-tasks"), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("scheduled-tasks-page")).is_some());
        window.click(name("source-board"), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("scheduled-tasks-page")).is_none());
        window.click(name("scheduled-tasks"), cx);
        window.render_frame(cx);
        assert!(!workspace.read(cx).source_board_open);
        assert!(window.try_find(name("scheduled-tasks-page")).is_some());
        window.click(name("scheduled-tasks"), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("scheduled-tasks-page")).is_none());
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn a_failed_read_is_an_error_never_an_empty_list(cx: &mut TestAppContext) {
    let fail = Arc::new(AtomicBool::new(true));
    let failing = fail.clone();
    let server = Scripted::new(move |request| {
        if failing.load(Ordering::Acquire) {
            return (500, json!({"error": "Failed to load scheduled tasks"}));
        }
        match request.path.as_str() {
            LIST => (200, json!({"tasks": []})),
            _ => (404, json!({})),
        }
    });
    let network = Network::new().expect("a network runtime");
    let (window, page) = open_page(cx, &server, &network);
    wait(cx, |cx| painted(cx, window, "scheduled-tasks-failed"));
    assert!(!painted(cx, window, "scheduled-tasks-empty"));
    assert_eq!(
        cx.read(|cx| page.read(cx).selected.clone()).as_deref(),
        Some(PROJECT),
        "the page opens on the first project"
    );
    fail.store(false, Ordering::Release);
    cx.update_window(window, |_, window, cx| window.click(name("scheduled-tasks-retry"), cx))
        .expect("window is open");
    wait(cx, |cx| painted(cx, window, "scheduled-tasks-empty"));
}

#[gpui_kit::test]
fn toggles_write_the_whole_task_back_and_runs_report_busy(cx: &mut TestAppContext) {
    let enabled = Arc::new(Mutex::new(true));
    let state = enabled.clone();
    let deleted = Arc::new(AtomicBool::new(false));
    let gone = deleted.clone();
    let server = Scripted::new(move |request| match (request.method.as_str(), request.path.as_str()) {
        ("GET", LIST) if gone.load(Ordering::Acquire) => (200, json!({"tasks": []})),
        ("GET", LIST) => (200, json!({"tasks": [stored_task(*state.lock().expect("state"))]})),
        ("PUT", LIST) => {
            *state.lock().expect("state") = request.body["task"]["enabled"] == json!(true);
            (200, json!({"tasks": [stored_task(*state.lock().expect("state"))]}))
        }
        ("POST", "/api/projects/proj-1/scheduled-tasks/t1/run") => {
            (409, json!({"error": "busy", "busy": "queued"}))
        }
        ("DELETE", "/api/projects/proj-1/scheduled-tasks/t1") => {
            gone.store(true, Ordering::Release);
            (200, json!({"tasks": []}))
        }
        _ => (404, json!({})),
    });
    let network = Network::new().expect("a network runtime");
    let (window, page) = open_page(cx, &server, &network);
    wait(cx, |cx| painted(cx, window, "scheduled-task:t1"));
    assert!(painted(cx, window, "scheduled-task-error:t1"), "the last error shows");

    cx.update_window(window, |_, window, cx| window.click(name("scheduled-task-toggle:t1"), cx))
        .expect("window is open");
    wait(cx, |cx| {
        cx.read(|cx| page.read(cx).task("t1").is_some_and(|task| !task.enabled))
            && cx.read(|cx| page.read(cx).mutating.is_none())
    });
    let saves = server.requests("PUT", LIST);
    assert_eq!(saves.len(), 1);
    assert_eq!(saves[0].body["task"]["enabled"], json!(false));
    assert_eq!(
        saves[0].body["task"]["futureField"],
        json!("kept"),
        "fields this client does not know go back unchanged"
    );

    cx.update_window(window, |_, window, cx| window.click(name("scheduled-task-run:t1"), cx))
        .expect("window is open");
    wait(cx, |cx| toast(cx, &page).is_some_and(|text| text.contains("waiting in the queue")));

    cx.update_window(window, |_, window, cx| {
        page.update(cx, |page, cx| page.delete("t1", window, cx))
    })
    .expect("window is open");
    wait(cx, |cx| painted(cx, window, "scheduled-tasks-empty"));
    assert_eq!(
        server
            .requests("DELETE", "/api/projects/proj-1/scheduled-tasks/t1")
            .len(),
        1
    );
}

#[gpui_kit::test]
fn the_editor_refuses_invalid_input_and_keeps_a_refused_save_open(cx: &mut TestAppContext) {
    let refuse = Arc::new(AtomicBool::new(true));
    let refusing = refuse.clone();
    let server = Scripted::new(move |request| match (request.method.as_str(), request.path.as_str()) {
        ("GET", LIST) if !refusing.load(Ordering::Acquire) => {
            (200, json!({"tasks": [stored_task(true)]}))
        }
        ("GET", LIST) => (200, json!({"tasks": []})),
        ("PUT", LIST) if refusing.load(Ordering::Acquire) => {
            (400, json!({"error": "Invalid timezone"}))
        }
        ("PUT", LIST) => (200, json!({"tasks": [stored_task(true)]})),
        _ => (404, json!({})),
    });
    let network = Network::new().expect("a network runtime");
    let (window, page) = open_page(cx, &server, &network);
    wait(cx, |cx| painted(cx, window, "scheduled-tasks-empty"));
    cx.update_window(window, |_, window, cx| window.click(name("scheduled-tasks-new"), cx))
        .expect("window is open");
    wait(cx, |cx| painted(cx, window, "scheduled-task-editor"));
    let editor = cx
        .read(|cx| page.read(cx).editor.clone())
        .expect("the editor is open");

    // Nothing named yet: the save is refused before any request.
    cx.update_window(window, |_, window, cx| window.click(name("scheduled-task-save"), cx))
        .expect("window is open");
    assert!(painted(cx, window, "scheduled-task-editor-error"));
    assert!(server.requests("PUT", LIST).is_empty());

    cx.update_window(window, |_, window, cx| {
        editor.update(cx, |editor, cx| {
            editor
                .name
                .update(cx, |input, cx| input.set_value("Nightly digest", window, cx));
            editor
                .prompt
                .update(cx, |input, cx| input.set_value("Summarize", window, cx));
            // Programmatic values raise no change event; the draft takes them
            // as typing would.
            editor.draft.name = "Nightly digest".into();
            editor.draft.prompt = "Summarize".into();
            editor.draft.provider_id = "p".into();
            editor.draft.model_id = "m".into();
            editor.set_kind(ScheduleKind::Cron, window, cx);
            editor.set_cron("61 * * * *", window, cx);
        })
    })
    .expect("window is open");
    assert!(painted(cx, window, "scheduled-task-cron-invalid"));
    cx.update_window(window, |_, window, cx| {
        editor.update(cx, |editor, cx| {
            editor.set_cron("0 9 * * 1", window, cx);
            editor.draft.timezone = "UTC".into();
        })
    })
    .expect("window is open");
    assert!(painted(cx, window, "scheduled-task-next-runs"));

    cx.update_window(window, |_, window, cx| window.click(name("scheduled-task-save"), cx))
        .expect("window is open");
    wait(cx, |cx| cx.read(|cx| !editor.read(cx).saving) && !server.requests("PUT", LIST).is_empty());
    let saved = &server.requests("PUT", LIST)[0].body["task"];
    assert_eq!(saved["schedule"], json!({"kind": "cron", "timezone": "UTC", "cron": "0 9 * * 1"}));
    assert_eq!(saved["name"], json!("Nightly digest"));
    assert_eq!(
        cx.read(|cx| editor.read(cx).error.as_ref().map(|error| error.to_string())),
        Some("Invalid timezone".to_owned()),
        "the refusal keeps the dialog and its form"
    );
    assert!(painted(cx, window, "scheduled-task-editor"));

    refuse.store(false, Ordering::Release);
    cx.update_window(window, |_, window, cx| window.click(name("scheduled-task-save"), cx))
        .expect("window is open");
    wait(cx, |cx| painted(cx, window, "scheduled-task:t1"));
    assert!(!painted(cx, window, "scheduled-task-editor"), "a landed save closes the dialog");
    assert!(matches!(cx.read(|cx| page.read(cx).list.clone()), TaskList::Ready(tasks) if tasks.len() == 1));
}
