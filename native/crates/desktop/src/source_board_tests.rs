//! The source board through real rendering and the workspace's sidebar,
//! against a scripted loopback server that answers the source-control routes
//! as the server's GitHub provider would and records every write.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext, ElementId, Entity, SharedString, TestAppContext};
use openchamber_native_core::source_board::RepositoryKind;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use serde_json::{Value, json};

use crate::network::Network;
use crate::source_board::{BoardProject, ProjectSource, SourceBoard};

const ROOT: &str = "/work/project";

#[derive(Clone, Debug)]
struct Request {
    method: String,
    path: String,
    body: Value,
}

#[derive(Default)]
struct World {
    signed_in: bool,
    fail_comments: bool,
    requests: Vec<Request>,
}

impl World {
    fn answer(&mut self, request: &Request) -> (u16, Value) {
        let issue = |number: u64, title: &str| {
            json!({"kind": "issue", "number": number, "title": title,
                "url": format!("https://github.com/acme/widgets/issues/{number}"), "body": "Body",
                "bodyTruncated": false, "createdAt": null, "updatedAt": "2026-11-01T10:00:00Z",
                "author": {"login": "octo"}, "labels": [], "commentCount": 0,
                "sourceRepo": {"owner": "acme", "repo": "widgets", "source": "origin"}, "state": "open"})
        };
        match request.path.as_str() {
            "/api/source-control/providers" => (
                200,
                json!({"providers": [{"id": "github", "label": "GitHub", "order": 0,
                    "terminology": {"changeRequest": "pullRequest", "changeRequestPrefix": "#", "issuePrefix": "#"},
                    "features": {"referenceSearch": true}}]}),
            ),
            "/api/source-control/binding" => (
                200,
                json!({"repository": {"repositoryId": "repo-1", "configRevision": "c", "remotes": [
                    {"name": "origin",
                     "fetch": {"displayUrl": "https://github.com/acme/widgets.git", "fingerprint": "f"},
                     "push": {"displayUrl": "https://github.com/acme/widgets.git", "fingerprint": "f"}}]},
                    "revision": 0, "binding": null}),
            ),
            "/api/source-control/instances" => (
                200,
                json!({"instances": [{"provider": "github", "instance": "github.com"}]}),
            ),
            "/api/source-control/github/auth/status" => (
                200,
                json!({"provider": "github", "instance": "github.com",
                    "status": if self.signed_in { "connected" } else { "disconnected" },
                    "connected": self.signed_in,
                    "accounts": if self.signed_in { json!([{"id": "acct", "current": true, "status": "valid"}]) } else { json!([]) }}),
            ),
            "/api/source-control/github/references" => (
                200,
                json!({"connected": true, "repo": {"owner": "acme", "repo": "widgets", "url": "u"},
                    "items": [issue(1, "Crash on startup"), issue(2, "Add a dark theme")],
                    "cursor": null, "hasMore": false, "total": 2}),
            ),
            "/api/source-control/github/references/detail" => (
                200,
                json!({"connected": true, "detail": {"number": 1, "commentTotal": 0, "comments": [], "pull": null}}),
            ),
            "/api/source-control/github/issues/comment" if self.fail_comments => {
                (502, json!({"error": "GitHub is unavailable"}))
            }
            "/api/source-control/github/issues/comment" => (
                200,
                json!({"status": "succeeded", "replayed": false, "result": {},
                    "actor": {"provider": "github", "instance": "github.com", "providerAccountId": "github.com#1"},
                    "target": {"repositoryId": "repo-1", "bindingRevision": 0, "primaryRemote": "origin",
                        "project": {"id": "1", "owner": "acme", "name": "widgets"}, "number": 1}}),
            ),
            _ => (404, json!({"error": "unexpected route"})),
        }
    }
}

/// A loopback server answering by path; dropping it stops the worker.
struct Scripted {
    endpoint: Endpoint,
    world: Arc<Mutex<World>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Scripted {
    fn new(world: World) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener binds");
        listener.set_nonblocking(true).expect("nonblocking");
        let endpoint = Endpoint::parse(&format!(
            "http://{}",
            listener.local_addr().expect("address")
        ))
        .expect("endpoint");
        let world = Arc::new(Mutex::new(world));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopped) = (world.clone(), stop.clone());
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
                let (status, body) = {
                    let mut world = shared.lock().expect("world");
                    world.requests.push(request.clone());
                    world.answer(&request)
                };
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
            world,
            stop,
            worker: Some(worker),
        }
    }

    fn requests(&self, path: &str) -> Vec<Request> {
        self.world
            .lock()
            .expect("world")
            .requests
            .iter()
            .filter(|request| request.path == path)
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
fn wait(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
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

fn name(id: impl Into<String>) -> ElementId {
    ElementId::Name(SharedString::from(id.into()))
}

fn painted(cx: &mut TestAppContext, window: AnyWindowHandle, id: &str) -> bool {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.try_find(name(id)).is_some()
    })
    .expect("window is open")
}

/// The board in the workspace's window, reading `server` and `ROOT`.
fn open_board(
    cx: &mut TestAppContext,
    server: &Scripted,
    network: &Network,
) -> (AnyWindowHandle, Entity<SourceBoard>) {
    cx.executor().allow_parking();
    let (window, workspace) = crate::workspace_tests::open(cx);
    let client = ServerClient::new(server.endpoint.clone(), Credential::None).expect("client");
    let handle = network.handle();
    let board = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let owner = cx.entity().downgrade();
                let runtime = workspace.runtime.clone();
                let board = cx.new(|cx| {
                    SourceBoard::new(owner, runtime, window, cx).with_network(client, handle)
                });
                // Opened inside the workspace's own update, as its sidebar
                // button does: the board must not read the workspace then.
                board.update(cx, |board, cx| {
                    board.open(
                        vec![BoardProject {
                            path: ROOT.to_owned(),
                            label: "project".into(),
                        }],
                        None,
                        cx,
                    )
                });
                workspace.source_board = Some(board.clone());
                workspace.source_board_open = true;
                board
            })
        })
        .expect("window is open");
    (window, board)
}

#[gpui_kit::test]
fn the_sidebar_button_opens_the_board_in_place_of_the_archive(cx: &mut TestAppContext) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(name("source-board"), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("source-board-page")).is_some());
        window.click(name("archive"), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("source-board-page")).is_none());
        assert!(workspace.read(cx).archive_open);
        window.click(name("source-board"), cx);
        window.render_frame(cx);
        assert!(!workspace.read(cx).archive_open);
        assert!(window.try_find(name("source-board-page")).is_some());
        window.click(name("source-board"), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("source-board-page")).is_none());
    })
    .expect("window is open");
    // Without registered projects the board says so; it never reads a server.
    let board = cx
        .read(|cx| workspace.read(cx).source_board.clone())
        .expect("the board is kept");
    assert_eq!(
        cx.read(|cx| board.read(cx).source.clone()),
        ProjectSource::NoProject
    );
}

#[gpui_kit::test]
fn a_signed_out_host_asks_for_a_sign_in_and_reads_nothing_else(cx: &mut TestAppContext) {
    let server = Scripted::new(World::default());
    let network = Network::new().expect("a network runtime");
    let (window, board) = open_board(cx, &server, &network);
    wait(cx, |cx| {
        matches!(
            cx.read(|cx| board.read(cx).source.clone()),
            ProjectSource::Unauthenticated { .. }
        )
    });
    assert!(painted(cx, window, "source-board-state"));
    assert!(
        server
            .requests("/api/source-control/github/references")
            .is_empty()
    );
}

#[gpui_kit::test]
fn the_list_previews_its_first_row_and_a_failed_comment_keeps_its_text(cx: &mut TestAppContext) {
    let server = Scripted::new(World {
        signed_in: true,
        fail_comments: true,
        ..World::default()
    });
    let network = Network::new().expect("a network runtime");
    let (window, board) = open_board(cx, &server, &network);
    wait(cx, |cx| {
        painted(cx, window, "source-board-row:github:acme/widgets#2")
    });
    assert_eq!(
        cx.read(|cx| board.read(cx).previewed().map(|item| item.number)),
        Some(1)
    );
    let references = server.requests("/api/source-control/github/references");
    assert_eq!(references[0].method, "GET");
    assert_eq!(cx.read(|cx| board.read(cx).kind), RepositoryKind::Issue);

    cx.update_window(window, |_, window, cx| {
        board.update(cx, |board, cx| {
            board
                .reply
                .update(cx, |input, cx| input.set_value("Thanks", window, cx));
            board.comment(window, cx);
        })
    })
    .expect("window is open");
    wait(cx, |cx| cx.read(|cx| board.read(cx).busy.is_none()));
    let notice = cx.read(|cx| {
        board
            .read(cx)
            .notice
            .as_ref()
            .map(|notice| notice.text.to_string())
    });
    assert_eq!(
        notice.as_deref(),
        Some("Could not comment on #1: GitHub is unavailable")
    );
    assert_eq!(
        cx.read(|cx| board.read(cx).reply.read(cx).value().to_string()),
        "Thanks"
    );

    server.world.lock().expect("world").fail_comments = false;
    cx.update_window(window, |_, window, cx| {
        board.update(cx, |board, cx| board.comment(window, cx))
    })
    .expect("window is open");
    wait(cx, |cx| cx.read(|cx| board.read(cx).busy.is_none()));
    let notice = cx.read(|cx| {
        board
            .read(cx)
            .notice
            .as_ref()
            .map(|notice| notice.text.to_string())
    });
    assert_eq!(notice.as_deref(), Some("Commented on #1"));
    assert_eq!(
        cx.read(|cx| board.read(cx).reply.read(cx).value().to_string()),
        ""
    );
    let comments = server.requests("/api/source-control/github/issues/comment");
    assert_eq!(comments.len(), 2);
    // Each click carries its own key; the context is the one the list read with.
    assert_ne!(
        comments[0].body["idempotencyKey"],
        comments[1].body["idempotencyKey"]
    );
    assert_eq!(comments[1].body["accountId"], json!("acct"));
    assert_eq!(comments[1].body["directory"], json!(ROOT));
    assert_eq!(comments[1].body["target"]["number"], json!(1));
    assert_eq!(comments[1].body["body"], json!("Thanks"));
}
