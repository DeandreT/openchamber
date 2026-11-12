//! The Files, Git and Diff surfaces through real hit testing and keyboard
//! dispatch, against a scripted loopback server that keeps an in-memory
//! workspace and Git status, so every outcome is checked on the server side
//! too.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AnyWindowHandle, AppContext, Bounds, ElementId, Entity, SharedString, TestAppContext,
    WindowBounds, WindowOptions, point, px, size,
};
use openchamber_native_core::file_draft::SaveTrigger;
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_core::path;
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use serde_json::{Value, json};

use crate::diff_view::DiffView;
use crate::files_view::{FilesView, PanelScope};
use crate::git_view::GitView;
use crate::locale::Strings;
use crate::network::Network;
use crate::theme::{self, Appearance, Palette};

const ROOT: &str = "/work/project";

/// One received request.
#[derive(Clone, Debug)]
struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn param(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// The server's workspace and repository.
#[derive(Default)]
struct World {
    /// File path to content and mtime; folders are implied by paths.
    files: BTreeMap<String, (String, f64)>,
    /// Porcelain letters per path.
    status: Vec<(String, char, char)>,
    /// Requests answered with this status and error message, by route.
    fail: Vec<(&'static str, u16, &'static str)>,
    requests: Vec<Request>,
    lose_rename_response: bool,
    /// Paths whose revert the server refuses.
    revert_fail: Vec<String>,
    /// Paths whose patch is not a diff, and paths left out of a diffs answer.
    bad_patch: Vec<String>,
    omit_patch: Vec<String>,
}

impl World {
    fn write(&mut self, path: &str, content: &str) {
        let mtime = self.files.get(path).map_or(1.0, |(_, mtime)| mtime + 10.0);
        self.files
            .insert(path.to_owned(), (content.to_owned(), mtime));
    }

    fn listing(&self, folder: &str) -> Option<Value> {
        let prefix = format!("{}/", folder.trim_end_matches('/'));
        let mut entries: BTreeMap<String, bool> = BTreeMap::new();
        for path in self.files.keys() {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            match rest.split_once('/') {
                Some((dir, _)) => {
                    entries.insert(dir.to_owned(), true);
                }
                None => {
                    entries.entry(rest.to_owned()).or_insert(false);
                }
            }
        }
        if entries.is_empty() && folder != ROOT {
            return None;
        }
        Some(json!({
            "path": folder,
            "entries": entries.iter().map(|(name, dir)| json!({
                "name": name, "path": format!("{prefix}{name}"), "isDirectory": dir,
                "isFile": !dir, "isSymbolicLink": false,
            })).collect::<Vec<_>>(),
        }))
    }

    fn answer(&mut self, request: &Request) -> (u16, &'static str, String) {
        if let Some((_, status, error)) = self
            .fail
            .iter()
            .find(|(route, _, _)| request.path.ends_with(route))
        {
            return (
                *status,
                "application/json",
                json!({ "error": error }).to_string(),
            );
        }
        let ok = |value: Value| (200, "application/json", value.to_string());
        let path = request.param("path").unwrap_or_default().to_owned();
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/api/fs/list") => match self.listing(&path) {
                Some(listing) => ok(listing),
                None => (
                    404,
                    "application/json",
                    r#"{"error":"Directory not found","reason":"not-found"}"#.to_owned(),
                ),
            },
            ("GET", "/api/fs/stat") => match self.files.get(&path) {
                Some((content, mtime)) => ok(json!({
                    "path": path, "isFile": true, "size": content.len(), "mtimeMs": mtime,
                })),
                None if self.listing(&path).is_some() => (
                    400,
                    "application/json",
                    r#"{"error":"Specified path is not a file"}"#.to_owned(),
                ),
                None => ok(json!({ "path": path, "exists": false })),
            },
            ("GET", "/api/fs/read") => match self.files.get(&path) {
                Some((content, _)) => (200, "text/plain; charset=utf-8", content.clone()),
                None => (
                    404,
                    "application/json",
                    r#"{"error":"File not found"}"#.to_owned(),
                ),
            },
            ("POST", "/api/fs/write") => {
                let body = request.json();
                let target = body["path"].as_str().unwrap_or_default().to_owned();
                self.write(&target, body["content"].as_str().unwrap_or_default());
                ok(json!({ "success": true, "path": target }))
            }
            ("POST", "/api/fs/rename") => {
                let body = request.json();
                let (from, to) = (
                    body["oldPath"].as_str().unwrap_or_default().to_owned(),
                    body["newPath"].as_str().unwrap_or_default().to_owned(),
                );
                let taken = self.files.contains_key(&to)
                    || self
                        .files
                        .keys()
                        .any(|path| path.starts_with(&format!("{to}/")));
                if taken {
                    return (
                        409,
                        "application/json",
                        r#"{"error":"Destination path already exists","reason":"already-exists"}"#
                            .to_owned(),
                    );
                }
                let paths: Vec<_> = self
                    .files
                    .keys()
                    .filter(|path| **path == from || path.starts_with(&format!("{from}/")))
                    .cloned()
                    .collect();
                for path in paths {
                    let file = self.files.remove(&path).expect("renamed file");
                    self.files
                        .insert(format!("{to}{}", &path[from.len()..]), file);
                }
                ok(json!({ "success": true, "path": to }))
            }
            ("GET", "/api/git/status") => ok(json!({
                "current": "main", "tracking": "origin/main", "ahead": 0, "behind": 0, "unborn": false,
                "files": self.status.iter().map(|(path, index, working)| json!({
                    "path": path, "index": index.to_string(), "working_dir": working.to_string(),
                })).collect::<Vec<_>>(),
                "isClean": self.status.is_empty(), "diffStats": { "staged": {}, "working": {} },
                "mergeInProgress": null, "rebaseInProgress": null,
            })),
            ("GET", "/api/git/diff") => ok(json!({
                "diff": format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-old\n+new\n"),
                "submodule": null,
            })),
            ("POST", "/api/git/diffs") => {
                let paths = request.json()["paths"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let diffs: Vec<Value> = paths
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|path| !self.omit_patch.iter().any(|omitted| omitted == path))
                    .map(|path| {
                        if self.bad_patch.iter().any(|bad| bad == path) {
                            return json!({ "path": path, "diff": "not a patch\n" });
                        }
                        json!({
                            "path": path,
                            "diff": format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-old\n+new\n"),
                        })
                    })
                    .collect();
                ok(json!({ "diffs": diffs }))
            }
            ("POST", "/api/git/stage") => {
                for path in request.json()["paths"].as_array().into_iter().flatten() {
                    if let Some(entry) = self
                        .status
                        .iter_mut()
                        .find(|(candidate, ..)| Some(candidate.as_str()) == path.as_str())
                    {
                        entry.1 = if entry.2 == '?' { 'A' } else { entry.2 };
                        entry.2 = ' ';
                    }
                }
                ok(json!({ "success": true }))
            }
            ("POST", "/api/git/revert") => {
                let target = request.json()["path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                if self.revert_fail.contains(&target) {
                    return (
                        500,
                        "application/json",
                        json!({ "error": format!("cannot revert {target}") }).to_string(),
                    );
                }
                let working = request.json()["scope"] == "working";
                self.status.retain_mut(|(candidate, index, worktree)| {
                    if *candidate != target {
                        return true;
                    }
                    if working && *index != ' ' && *index != '?' {
                        *worktree = ' ';
                        true
                    } else {
                        false
                    }
                });
                ok(json!({ "success": true }))
            }
            _ => (
                404,
                "application/json",
                r#"{"error":"unscripted"}"#.to_owned(),
            ),
        }
    }
}

struct Scripted {
    endpoint: Endpoint,
    world: Arc<Mutex<World>>,
    stop: Arc<AtomicBool>,
    held_route: Arc<Mutex<Option<String>>>,
    held_rename_reply: Arc<AtomicBool>,
    released_rename_reply: Arc<AtomicBool>,
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
        let held_route = Arc::new(Mutex::new(None::<String>));
        let held = held_route.clone();
        let held_rename_reply = Arc::new(AtomicBool::new(false));
        let hold_rename = held_rename_reply.clone();
        let released_rename_reply = Arc::new(AtomicBool::new(false));
        let released_rename = released_rename_reply.clone();
        let worker = thread::spawn(move || {
            let mut delayed_rename: Option<(TcpStream, u16, &'static str, String)> = None;
            while !stopped.load(Ordering::Acquire) {
                if !hold_rename.load(Ordering::Acquire)
                    && let Some((mut stream, status, kind, body)) = delayed_rename.take()
                {
                    write_response(&mut stream, status, kind, &body);
                    released_rename.store(true, Ordering::Release);
                }
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                };
                stream.set_nonblocking(false).expect("blocking stream");
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("read timeout");
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                shared.lock().expect("world").requests.push(request.clone());
                while held.lock().expect("gate").as_deref()
                    == Some(format!("before:{}", request.path).as_str())
                    && !stopped.load(Ordering::Acquire)
                {
                    thread::sleep(Duration::from_millis(1));
                }
                let (status, kind, body) = {
                    let mut world = shared.lock().expect("world");
                    world.answer(&request)
                };
                if request.path == "/api/fs/rename"
                    && shared.lock().expect("world").lose_rename_response
                {
                    continue;
                }
                if request.path == "/api/fs/rename"
                    && hold_rename.load(Ordering::Acquire)
                    && delayed_rename.is_none()
                {
                    delayed_rename = Some((stream, status, kind, body));
                    continue;
                }
                while held.lock().expect("gate").as_deref() == Some(request.path.as_str())
                    && !stopped.load(Ordering::Acquire)
                {
                    thread::sleep(Duration::from_millis(1));
                }
                write_response(&mut stream, status, kind, &body);
            }
        });
        Self {
            endpoint,
            world,
            stop,
            held_route,
            held_rename_reply,
            released_rename_reply,
            worker: Some(worker),
        }
    }

    fn requests(&self, method: &str, route: &str) -> Vec<Request> {
        self.world
            .lock()
            .expect("world")
            .requests
            .iter()
            .filter(|request| request.method == method && request.path == route)
            .cloned()
            .collect()
    }
}

fn write_response(stream: &mut TcpStream, status: u16, kind: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
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
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0];
        stream.read_exact(&mut byte).ok()?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let headers = String::from_utf8(bytes).ok()?;
    let mut first = headers.lines().next()?.split_whitespace();
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let length: usize = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| value.trim().parse().unwrap_or(0));
    let mut body = vec![0; length];
    stream.read_exact(&mut body).ok()?;
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    Some(Request {
        method,
        path: path.to_owned(),
        query: query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (decode(key), decode(value))
            })
            .collect(),
        body,
    })
}

/// `application/x-www-form-urlencoded` decoding of one query component.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let escaped = (byte == b'%')
            .then(|| value.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (byte, escaped) {
            (_, Some(decoded)) => {
                out.push(decoded);
                index += 3;
                continue;
            }
            (b'+', None) => out.push(b' '),
            (other, None) => out.push(other),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

/// Waits until the window paints the element `id`.
#[track_caller]
fn wait_for_element(cx: &mut TestAppContext, window: AnyWindowHandle, id: &str) {
    let id = id.to_owned();
    wait(cx, move |cx| {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(name(id.clone())).is_some()
        })
        .expect("window is open")
    });
}

fn name(id: impl Into<String>) -> ElementId {
    ElementId::Name(SharedString::from(id.into()))
}

struct Harness {
    _network: Network,
    scope: PanelScope,
}

fn harness(server: &Scripted) -> Harness {
    let network = Network::new().expect("a network runtime");
    let client = ServerClient::new(server.endpoint.clone(), Credential::None).expect("client");
    let scope = PanelScope {
        client,
        network: network.handle(),
        runtime: RuntimeId::from("panel-test"),
        directory: ROOT.to_owned(),
        owner: None,
    };
    Harness {
        _network: network,
        scope,
    }
}

fn open_view<V: gpui_kit::Render + 'static>(
    cx: &mut TestAppContext,
    build: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::Context<V>) -> V + 'static,
) -> (AnyWindowHandle, Entity<V>) {
    // Requests finish on the network runtime's threads.
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_global(Strings::load("en"));
        theme::apply(Palette::default_for(Appearance::Dark), cx);
        let (window, view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.0), px(0.0)),
                    size: size(px(640.0), px(700.0)),
                })),
                ..WindowOptions::default()
            },
            cx,
            |window, cx| {
                window.set_layout_mode(gpui_kit::LayoutMode::Logical);
                cx.new(|cx| build(window, cx))
            },
        )
        .expect("the test window opens");
        (window, view)
    })
}

fn frame(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .expect("window is open");
}

fn files_world() -> World {
    let mut world = World::default();
    world.write(&format!("{ROOT}/README.md"), "# Readme\n");
    world.write(&format!("{ROOT}/src/app.txt"), "one\r\ntwo\r\n");
    world.write(&format!("{ROOT}/src/lib.txt"), "lib\n");
    world
}

fn open_files(cx: &mut TestAppContext, harness: &Harness) -> (AnyWindowHandle, Entity<FilesView>) {
    let scope = harness.scope.clone();
    let shown = std::rc::Rc::new(std::cell::Cell::new(true));
    open_view(cx, move |window, cx| {
        FilesView::new(scope, shown, window, cx)
    })
}

#[gpui_kit::test]
fn the_tree_lists_folders_first_and_expands_by_keyboard(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    wait(cx, |cx| cx.read(|cx| !view.read(cx).tree.rows().is_empty()));
    frame(cx, window);
    let rows: Vec<String> = cx.read(|cx| {
        view.read(cx)
            .tree
            .rows()
            .into_iter()
            .map(|row| row.name)
            .collect()
    });
    assert_eq!(rows, ["src", "README.md"]);
    let listing = &server.requests("GET", "/api/fs/list")[0];
    assert_eq!(listing.param("path"), Some(ROOT));
    assert_eq!(listing.param("respectGitignore"), Some("true"));
    cx.update_window(window, |_, window, cx| {
        window.click(name(format!("files-row:{ROOT}/src")), cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    wait(cx, |cx| cx.read(|cx| view.read(cx).tree.rows().len() == 4));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let snapshot = window.find(name(format!("files-row:{ROOT}/src")));
        assert_eq!(snapshot.role(), Some(gpui_kit::Role::TreeItem));
        // Left folds the selected folder, Right opens it again.
        window.press("left", cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    assert_eq!(cx.read(|cx| view.read(cx).tree.rows().len()), 2);
    cx.update_window(window, |_, window, cx| {
        window.press("right", cx);
        window.press("down", cx);
        window.press("enter", cx);
        window.render_frame(cx);
    })
    .expect("window is open");
    wait(cx, |cx| {
        cx.read(|cx| {
            view.read(cx)
                .open
                .as_ref()
                .is_some_and(|open| !open.file.is_loading())
        })
    });
    let opened = cx.read(|cx| {
        view.read(cx)
            .open
            .as_ref()
            .map(|open| open.file.path().to_owned())
    });
    assert_eq!(
        opened.as_deref(),
        Some(format!("{ROOT}/src/app.txt").as_str())
    );
}

#[gpui_kit::test]
fn a_saved_edit_keeps_crlf_and_a_dirty_draft_survives_an_external_write(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    let path = format!("{ROOT}/src/app.txt");
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.open_file(&path, window, cx));
    })
    .expect("window is open");
    wait(cx, |cx| {
        cx.read(|cx| {
            view.read(cx)
                .open
                .as_ref()
                .is_some_and(|open| !open.file.is_loading())
        })
    });
    frame(cx, window);
    let editor = cx.read(|cx| view.read(cx).open.as_ref().expect("open").editor.clone());
    assert_eq!(
        cx.read(|cx| editor.read(cx).value().to_string()),
        "one\ntwo\n"
    );
    cx.update_window(window, |_, window, cx| {
        window.click(name("files-editor"), cx);
        window.press("ctrl-end", cx);
        window.input("three", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    assert_eq!(
        cx.read(|cx| editor.read(cx).value().to_string()),
        "one\ntwo\nthree"
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window
                .find(name("files-save-status"))
                .label()
                .map(str::to_owned),
            Some("Unsaved changes".to_owned())
        );
        window.press("ctrl-s", cx);
    })
    .expect("window is open");
    wait(cx, |_| !server.requests("POST", "/api/fs/write").is_empty());
    let write = server.requests("POST", "/api/fs/write").remove(0);
    assert_eq!(write.json()["content"], "one\r\ntwo\r\nthree");
    wait(cx, |cx| {
        cx.read(|cx| !view.read(cx).open.as_ref().expect("open").file.is_dirty())
    });

    // A dirty draft is never replaced by the file on disk.
    cx.update_window(window, |_, window, cx| {
        window.click(name("files-editor"), cx);
        window.input("!", cx);
    })
    .expect("window is open");
    server.world.lock().expect("world").write(&path, "theirs\n");
    for _ in 0..2 {
        cx.update(|cx| view.update(cx, |view, cx| view.poll_open_file(cx)));
        wait(cx, |cx| {
            cx.read(|cx| {
                view.read(cx)
                    .open
                    .as_ref()
                    .is_some_and(|open| open.file.failure().is_none())
            })
        });
    }
    assert_eq!(
        cx.read(|cx| view
            .read(cx)
            .open
            .as_ref()
            .expect("open")
            .file
            .draft()
            .to_owned()),
        "one\ntwo\nthree!"
    );
    cx.update(|cx| view.update(cx, |view, cx| view.save(SaveTrigger::Explicit, cx)));
    wait(cx, |_| server.requests("POST", "/api/fs/write").len() == 2);
    assert_eq!(
        server.world.lock().expect("world").files[&path].0,
        "one\r\ntwo\r\nthree!",
        "saving writes the draft over the external change"
    );
}

#[gpui_kit::test]
fn a_clean_file_takes_an_external_change_and_a_deleted_one_reports_it(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    let path = format!("{ROOT}/README.md");
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.open_file(&path, window, cx));
    })
    .expect("window is open");
    wait(cx, |cx| {
        cx.read(|cx| {
            view.read(cx)
                .open
                .as_ref()
                .is_some_and(|open| !open.file.is_loading())
        })
    });
    server
        .world
        .lock()
        .expect("world")
        .write(&path, "# Changed outside\n");
    let editor = cx.read(|cx| view.read(cx).open.as_ref().expect("open").editor.clone());
    wait(cx, |cx| {
        cx.update(|cx| view.update(cx, |view, cx| view.poll_open_file(cx)));
        frame(cx, window);
        cx.read(|cx| editor.read(cx).value() == "# Changed outside\n")
    });
    server.world.lock().expect("world").files.remove(&path);
    wait(cx, |cx| {
        cx.update(|cx| view.update(cx, |view, cx| view.poll_open_file(cx)));
        frame(cx, window);
        cx.read(|cx| {
            view.read(cx)
                .open
                .as_ref()
                .is_some_and(|open| open.file.failure().is_some())
        })
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(name("files-error")).is_some());
    })
    .expect("window is open");
    server.world.lock().expect("world").write(&path, "# Back\n");
    wait(cx, |cx| {
        cx.update(|cx| view.update(cx, |view, cx| view.poll_open_file(cx)));
        frame(cx, window);
        cx.read(|cx| editor_text(&view, cx) == Some("# Back\n".to_owned()))
    });
}

fn editor_text(view: &Entity<FilesView>, cx: &gpui_kit::App) -> Option<String> {
    let open = view.read(cx).open.as_ref()?;
    Some(open.editor.read(cx).value().to_string())
}

#[gpui_kit::test]
fn a_rename_onto_an_existing_name_replaces_nothing(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    wait(cx, |cx| cx.read(|cx| !view.read(cx).tree.rows().is_empty()));
    let path = format!("{ROOT}/README.md");
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.start_rename(&path, window, cx));
        window.render_frame(cx);
        window.press("ctrl-a", cx);
        window.input("src", cx);
        window.press("enter", cx);
    })
    .expect("window is open");
    wait(cx, |cx| cx.read(|cx| view.read(cx).notice.is_some()));
    let notice = cx.read(|cx| {
        view.read(cx)
            .notice
            .clone()
            .expect("a notice")
            .text
            .to_string()
    });
    assert_eq!(notice, "project already has a file or folder named src");
    assert!(
        server
            .world
            .lock()
            .expect("world")
            .files
            .contains_key(&path)
    );
    let _ = window;
}

fn git_world() -> World {
    World {
        status: vec![
            ("a.txt".to_owned(), 'M', 'M'),
            ("b.txt".to_owned(), ' ', 'M'),
            ("new.txt".to_owned(), '?', '?'),
        ],
        ..World::default()
    }
}

fn open_git(cx: &mut TestAppContext, harness: &Harness) -> (AnyWindowHandle, Entity<GitView>) {
    let scope = harness.scope.clone();
    open_view(cx, move |window, cx| GitView::new(scope, window, cx))
}

#[gpui_kit::test]
fn update_branch_tiles_fit_equal_columns_and_wrap_in_both_directions(cx: &mut TestAppContext) {
    let server = Scripted::new(git_world());
    let harness = harness(&server);
    let (window, _view) = open_git(cx, &harness);
    wait_for_element(cx, window, "git-repository-views");
    cx.update_window(window, |_, window, cx| {
        window.click(name("git-repository-views"), cx);
        window.render_frame(cx);
        window.click(name("git-update-branch"), cx);
    })
    .expect("window is open");
    wait_for_element(cx, window, "git-update-dialog");

    let mut failures = Vec::new();
    for language in ["en", "ar"] {
        for width in [640.0, crate::pane_resize::PANEL_MIN_WIDTH] {
            cx.simulate_window_resize(window, size(px(width), px(700.0)));
            cx.update_window(window, |_, window, cx| {
                cx.set_global(Strings::load(language));
                window.refresh();
                window.render_frame(cx);
                let dialog = window.find("git-update-dialog").bounds();
                let merge = window.find("git-update-merge").bounds();
                let rebase = window.find("git-update-rebase").bounds();
                let expected = (dialog.size.width - px(32.0 + 12.0)) / 2.0;
                let line_height = px(
                    crate::context_panel::XS_LINE * crate::appearance::root_font_size(cx),
                );
                for (id, tile, description) in [
                    ("merge", merge, "gitView.branch.mergeDescription"),
                    ("rebase", rebase, "gitView.branch.rebaseDescription"),
                ] {
                    let text = window.find(description).bounds();
                    eprintln!("{language} {width}px {id}: dialog={dialog:?} tile={tile:?} description={text:?}");
                    let checks = [
                        (
                            "equal column width",
                            (f32::from(tile.size.width - expected)).abs() < 0.01,
                        ),
                        (
                            "tile within dialog padding",
                            tile.left() >= dialog.left() + px(16.0)
                                && tile.right() <= dialog.right() - px(16.0),
                        ),
                        (
                            "description within tile padding",
                            text.left() >= tile.left() + px(12.0)
                                && text.right() <= tile.right() - px(12.0)
                                && text.bottom() <= tile.bottom() - px(12.0) + px(1.0),
                        ),
                        ("description wraps", text.size.height > line_height),
                    ];
                    for (check, passed) in checks {
                        if !passed {
                            failures.push(format!("{language} {width}px {id}: {check}"));
                        }
                    }
                }
                let gap = if language == "ar" {
                    merge.left() - rebase.right()
                } else {
                    rebase.left() - merge.right()
                };
                assert_eq!(gap, px(12.0), "{language} preserves the mirrored tile gap");
                assert_eq!(
                    window.find("git-update-merge").checked(),
                    Some(true),
                    "layout and locale changes preserve the selected operation"
                );
            })
            .expect("window is open");
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[gpui_kit::test]
fn a_partly_staged_file_shows_in_both_scopes_and_staging_follows_the_server(
    cx: &mut TestAppContext,
) {
    let server = Scripted::new(git_world());
    let harness = harness(&server);
    let (window, _view) = open_git(cx, &harness);
    wait_for_element(cx, window, "git-change:staged:a.txt");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(name("git-change:staged:a.txt")).is_some());
        assert!(window.try_find(name("git-change:unstaged:a.txt")).is_some());
        assert!(
            window
                .try_find(name("git-change:unstaged:new.txt"))
                .is_some()
        );
        window.click(name("git-stage:new.txt"), cx);
    })
    .expect("window is open");
    wait_for_element(cx, window, "git-change:staged:new.txt");
    let stage = server.requests("POST", "/api/git/stage").remove(0);
    assert_eq!(stage.json(), json!({ "paths": ["new.txt"] }));
    assert_eq!(stage.param("directory"), Some(ROOT));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(name("git-change:staged:new.txt")).is_some());
        assert!(
            window
                .try_find(name("git-change:unstaged:new.txt"))
                .is_none()
        );
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn discard_asks_first_and_a_failed_stage_reads_the_status_again(cx: &mut TestAppContext) {
    let server = Scripted::new(git_world());
    let harness = harness(&server);
    let (window, view) = open_git(cx, &harness);
    let click = |cx: &mut TestAppContext, id: &str| {
        wait_for_element(cx, window, id);
        cx.update_window(window, |_, window, cx| window.click(name(id), cx))
            .expect("window is open");
    };
    click(cx, "git-discard:b.txt");
    click(cx, "git-discard-cancel");
    wait_for_element(cx, window, "git-discard:b.txt");
    assert!(
        server.requests("POST", "/api/git/revert").is_empty(),
        "cancel sends nothing"
    );
    click(cx, "git-discard:b.txt");
    click(cx, "git-discard-confirm");
    wait(cx, |_| {
        server.requests("POST", "/api/git/revert").len() == 1
    });
    wait(cx, |cx| cx.read(|cx| view.read(cx).notice_text().is_some()));
    server.world.lock().expect("world").fail.push((
        "/api/git/stage",
        500,
        "Unable to create index.lock: File exists.",
    ));
    let reads = server.requests("GET", "/api/git/status").len();
    click(cx, "git-stage:a.txt");
    wait(cx, |cx| {
        cx.read(|cx| view.read(cx).notice_text())
            .is_some_and(|notice| notice.starts_with("Failed to stage"))
    });
    wait(cx, |_| {
        server.requests("GET", "/api/git/status").len() > reads
    });
    cx.run_until_parked();
    let notice = cx.read(|cx| view.read(cx).notice_text());
    assert_eq!(
        notice.as_deref(),
        Some("Failed to stage changes: Unable to create index.lock: File exists.")
    );
    wait_for_element(cx, window, "git-change:unstaged:a.txt");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(name("git-change:unstaged:b.txt")).is_none());
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn the_rail_opens_and_closes_server_surfaces(cx: &mut TestAppContext) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(name("rail-files"), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("side-panel")).is_some());
        window.click(name("rail-git"), cx);
        window.render_frame(cx);
        window.click(name("rail-git"), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("side-panel")).is_none());
        window.click(name("rail-diff"), cx);
        window.render_frame(cx);
        window.click(name("rail-context"), cx);
        window.render_frame(cx);
        assert!(window.try_find(name("side-panel")).is_none());
    })
    .expect("window is open");
    assert_eq!(cx.read(|cx| workspace.read(cx).panels.open), None);
}

#[gpui_kit::test]
fn dirty_switch_cancel_failed_save_and_discard_keep_the_draft(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    let path = format!("{ROOT}/src/lib.txt");
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.open_file(&path, window, cx));
    })
    .expect("window is open");
    wait(cx, |cx| {
        cx.read(|cx| {
            view.read(cx)
                .open
                .as_ref()
                .is_some_and(|open| !open.file.is_loading())
        })
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(name("files-editor"), cx);
        window.press("ctrl-end", cx);
        window.input("kept", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    let other = format!("{ROOT}/README.md");
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.open_file(&other, window, cx));
    })
    .expect("window is open");
    assert!(cx.read(|cx| view.read(cx).switch.is_some()));
    assert!(server.requests("POST", "/api/fs/write").is_empty());
    cx.update(|cx| view.update(cx, |view, cx| view.cancel_switch(cx)));
    assert_eq!(
        cx.read(|cx| editor_text(&view, cx)),
        Some("lib\nkept".into())
    );
    server
        .world
        .lock()
        .expect("world")
        .fail
        .push(("/api/fs/write", 500, "write refused"));
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.open_file(&other, window, cx);
            view.save_and_switch(cx);
        });
    })
    .expect("window");
    wait(cx, |cx| cx.read(|cx| view.read(cx).notice.is_some()));
    frame(cx, window);
    assert_eq!(
        cx.read(|cx| editor_text(&view, cx)),
        Some("lib\nkept".into())
    );
    assert!(cx.read(|cx| view.read(cx).switch.is_some()));
    assert_eq!(server.world.lock().expect("world").files[&path].0, "lib\n");
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.discard_and_switch(window, cx));
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| editor_text(&view, cx)) == Some("# Readme\n".into())
    });
}

fn load_file(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<FilesView>,
    path: &str,
) {
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.open_file(path, window, cx))
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| {
            view.read(cx)
                .open
                .as_ref()
                .is_some_and(|open| !open.file.is_loading())
        })
    });
}

fn edit_text(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<FilesView>,
    text: &str,
) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(name("files-editor"), cx);
        window.press("ctrl-a", cx);
        window.press("backspace", cx);
        if !text.is_empty() {
            window.input(text, cx);
        }
    })
    .expect("window");
    cx.run_until_parked();
    assert_eq!(
        cx.read(|cx| view
            .read(cx)
            .open
            .as_ref()
            .expect("open")
            .file
            .draft()
            .to_owned()),
        text
    );
}

#[gpui_kit::test]
fn dirty_switch_saves_an_empty_draft_and_waits_for_the_write(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    let path = format!("{ROOT}/README.md");
    load_file(cx, window, &view, &path);
    edit_text(cx, window, &view, "");
    *server.held_route.lock().expect("gate") = Some("/api/fs/write".into());
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.open_file(&format!("{ROOT}/src/lib.txt"), window, cx);
            view.save_and_switch(cx);
        })
    })
    .expect("window");
    wait(cx, |_| !server.requests("POST", "/api/fs/write").is_empty());
    cx.update(|cx| view.update(cx, |view, cx| view.save_and_switch(cx)));
    frame(cx, window);
    assert_eq!(
        cx.read(|cx| view
            .read(cx)
            .open
            .as_ref()
            .expect("open")
            .file
            .path()
            .to_owned()),
        path
    );
    *server.held_route.lock().expect("gate") = None;
    wait(cx, |cx| {
        frame(cx, window);
        cx.read(|cx| editor_text(&view, cx)) == Some("lib\n".into())
    });
    assert_eq!(server.world.lock().expect("world").files[&path].0, "");
    assert_eq!(server.requests("POST", "/api/fs/write").len(), 1);
}

#[gpui_kit::test]
fn edits_during_save_rearm_autosave(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    let path = format!("{ROOT}/README.md");
    load_file(cx, window, &view, &path);
    edit_text(cx, window, &view, "first");
    *server.held_route.lock().expect("gate") = Some("/api/fs/write".into());
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.save(SaveTrigger::Explicit, cx);
        })
    });
    wait(cx, |_| !server.requests("POST", "/api/fs/write").is_empty());
    edit_text(cx, window, &view, "second");
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    *server.held_route.lock().expect("gate") = None;
    wait(cx, |cx| {
        cx.read(|cx| !view.read(cx).open.as_ref().expect("open").file.is_saving())
    });
    cx.executor().advance_clock(Duration::from_secs(2));
    wait(cx, |_| server.requests("POST", "/api/fs/write").len() == 2);
    assert_eq!(server.world.lock().expect("world").files[&path].0, "second");
}

#[gpui_kit::test]
fn rename_moves_parent_prefix_and_inflight_load_completes(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    *server.held_route.lock().expect("gate") = Some("before:/api/fs/read".into());
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.open_file(&format!("{ROOT}/src/lib.txt"), window, cx)
        })
    })
    .expect("window");
    wait(cx, |_| !server.requests("GET", "/api/fs/read").is_empty());
    {
        let mut world = server.world.lock().expect("world");
        let file = world
            .files
            .remove(&format!("{ROOT}/src/lib.txt"))
            .expect("source");
        world.files.insert(format!("{ROOT}/renamed/lib.txt"), file);
    }
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.path_moved(&format!("{ROOT}/src"), &format!("{ROOT}/renamed"), cx)
        })
    });
    *server.held_route.lock().expect("gate") = None;
    wait(cx, |cx| {
        cx.read(|cx| !view.read(cx).open.as_ref().expect("open").file.is_loading())
    });
    assert_eq!(
        cx.read(|cx| view
            .read(cx)
            .open
            .as_ref()
            .expect("open")
            .file
            .path()
            .to_owned()),
        format!("{ROOT}/renamed/lib.txt")
    );
    edit_text(cx, window, &view, "changed");
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.renaming.push(format!("{ROOT}/renamed"));
            assert_eq!(
                view.save(SaveTrigger::Explicit, cx),
                crate::files_view::SaveStart::Refused
            );
            view.rename_finished(&format!("{ROOT}/renamed"), cx);
            assert_eq!(
                view.save(SaveTrigger::Explicit, cx),
                crate::files_view::SaveStart::Started
            );
        })
    });
    wait(cx, |_| !server.requests("POST", "/api/fs/write").is_empty());
    assert_eq!(
        server.requests("POST", "/api/fs/write")[0].json()["path"],
        format!("{ROOT}/renamed/lib.txt")
    );
}

#[gpui_kit::test]
fn retained_panel_scope_uses_live_connection_and_refuses_disconnected_owner(
    cx: &mut TestAppContext,
) {
    let first = Scripted::new(files_world());
    let second = Scripted::new(files_world());
    let harness = harness(&first);
    let (window, workspace) = crate::workspace_tests::open(cx);
    let config = crate::connection::ServerConfig {
        endpoint: first.endpoint.clone(),
        password: None,
        client: openchamber_native_transport::client::ClientDescription {
            label: "test".into(),
            kind: "desktop".into(),
            dedupe_key: None,
            device_platform: None,
            app_version: None,
        },
    };
    let mut connection = crate::connection::Connection::new(config).expect("network");
    connection.client = Some(harness.scope.client.clone());
    cx.update(|cx| workspace.update(cx, |workspace, _| workspace.connection = Some(connection)));
    let mut scope = harness.scope.clone();
    scope.owner = Some(workspace.downgrade());
    scope.runtime = cx.read(|cx| workspace.read(cx).runtime.clone());
    assert!(cx.read(|cx| scope.client(cx)).is_some());
    cx.update(|cx| {
        workspace.update(cx, |workspace, _| {
            workspace.connection.as_mut().expect("connection").restart();
        })
    });
    assert!(
        cx.read(|cx| scope.client(cx)).is_none(),
        "hidden views cannot reuse a revoked client"
    );
    let next = ServerClient::new(second.endpoint.clone(), Credential::None).expect("client");
    cx.update(|cx| {
        workspace.update(cx, |workspace, _| {
            workspace.connection.as_mut().expect("connection").client = Some(next)
        })
    });
    let client = cx.read(|cx| scope.client(cx)).expect("reconnected");
    assert_eq!(
        client.endpoint(),
        &second.endpoint,
        "all retained scopes resolve the new connection"
    );
    cx.executor().allow_parking();
    cx.update(|cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.selected = None;
            workspace
                .draft_projects
                .insert(workspace.runtime.clone(), ROOT.into());
            workspace.panels.open = Some(crate::files_view::PanelSurface::Files);
            cx.notify();
        })
    });
    wait_for_element(cx, window, &format!("files-row:{ROOT}/README.md"));
    assert!(!second.requests("GET", "/api/fs/list").is_empty());
    cx.update(|cx| {
        workspace.update(cx, |workspace, _| {
            workspace.runtime = RuntimeId::from("other")
        })
    });
    assert!(cx.read(|cx| scope.client(cx)).is_none());
}

fn lost_rename_response(cx: &mut TestAppContext, folder: bool) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    let old = format!("{ROOT}/src{}", if folder { "" } else { "/lib.txt" });
    let new = if folder {
        format!("{ROOT}/moved")
    } else {
        format!("{ROOT}/src/moved")
    };
    let original = format!("{ROOT}/src/lib.txt");
    let destination = if folder {
        format!("{new}/lib.txt")
    } else {
        new.clone()
    };
    load_file(cx, window, &view, &original);
    if !folder {
        cx.update(|cx| {
            view.update(cx, |view, cx| {
                view.toggle_folder(&format!("{ROOT}/src"), cx)
            })
        });
        wait_for_element(cx, window, &format!("files-row:{original}"));
    }
    edit_text(cx, window, &view, "retained draft");
    {
        let mut world = server.world.lock().expect("world");
        world.lose_rename_response = true;
        world.fail.push(("/api/fs/list", 500, "observation failed"));
    }
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.start_rename(&old, window, cx));
        window.render_frame(cx);
        window.press("ctrl-a", cx);
        window.input("moved", cx);
        window.press("enter", cx);
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| !view.read(cx).uncertain_renames.is_empty())
    });
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(
        server.requests("POST", "/api/fs/write").is_empty(),
        "unknown outcome blocks autosave"
    );
    assert!(
        !server
            .world
            .lock()
            .expect("world")
            .files
            .contains_key(&original)
    );
    assert_eq!(
        cx.read(|cx| editor_text(&view, cx)),
        Some("retained draft".into())
    );
    {
        let mut world = server.world.lock().expect("world");
        world.fail.clear();
        world.write(&original, "ambiguous source");
    }
    cx.update(|cx| view.update(cx, |view, cx| view.refresh_tree(cx)));
    wait(cx, |_| {
        server
            .requests("GET", "/api/fs/list")
            .iter()
            .any(|request| request.param("path") == path::parent(&new).as_deref())
    });
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            assert_eq!(
                view.save(SaveTrigger::Explicit, cx),
                crate::files_view::SaveStart::Refused
            );
        })
    });
    server.world.lock().expect("world").files.remove(&original);
    cx.update(|cx| view.update(cx, |view, cx| view.refresh_tree(cx)));
    wait(cx, |cx| {
        cx.read(|cx| view.read(cx).uncertain_renames.is_empty())
    });
    cx.executor().advance_clock(Duration::from_secs(2));
    wait(cx, |_| !server.requests("POST", "/api/fs/write").is_empty());
    let world = server.world.lock().expect("world");
    assert!(
        !world.files.contains_key(&original),
        "autosave must not recreate the source"
    );
    assert_eq!(world.files[&destination].0, "retained draft");
}

#[gpui_kit::test]
fn lost_file_rename_response_blocks_writes_until_reconciled(cx: &mut TestAppContext) {
    lost_rename_response(cx, false);
}

#[gpui_kit::test]
fn lost_folder_rename_response_blocks_writes_until_reconciled(cx: &mut TestAppContext) {
    lost_rename_response(cx, true);
}

#[gpui_kit::test]
fn unresolved_descendant_rename_blocks_ancestor_renames(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (_, view) = open_files(cx, &harness);
    cx.update(|cx| {
        view.update(cx, |view, _| {
            view.uncertain_renames.insert(
                format!("{ROOT}/src/lib.txt"),
                format!("{ROOT}/src/renamed.txt"),
            );
            assert!(
                view.save_blocks_rename(&format!("{ROOT}/src")),
                "moving an ancestor must not bypass the unresolved descendant's write block"
            );
            assert!(view.save_blocks_rename(&format!("{ROOT}/src/renamed.txt")));
            assert!(!view.save_blocks_rename(&format!("{ROOT}/src-other")));
        });
    });
}

#[gpui_kit::test]
fn observing_an_unmoved_source_does_not_release_an_uncertain_rename(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    let old = format!("{ROOT}/src/lib.txt");
    let new = format!("{ROOT}/src/moved.txt");
    load_file(cx, window, &view, &old);
    edit_text(cx, window, &view, "retained late-rename draft");
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.check_rename(old.clone(), new.clone(), cx)
        })
    });
    wait(cx, |_| {
        server
            .requests("GET", "/api/fs/list")
            .iter()
            .filter(|request| request.param("path") == Some(&format!("{ROOT}/src")))
            .count()
            >= 2
    });
    cx.run_until_parked();
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            assert!(
                view.uncertain_renames.contains_key(&old),
                "existence alone cannot settle an outstanding rename"
            );
            assert_eq!(
                view.save(SaveTrigger::Explicit, cx),
                crate::files_view::SaveStart::Refused
            );
        })
    });
    // The first server operation commits after the observation above.
    {
        let mut world = server.world.lock().expect("world");
        let file = world.files.remove(&old).expect("source file");
        world.files.insert(new.clone(), file);
    }
    cx.update(|cx| view.update(cx, |view, cx| view.refresh_tree(cx)));
    wait(cx, |cx| {
        cx.read(|cx| view.read(cx).uncertain_renames.is_empty())
    });
    cx.executor().advance_clock(Duration::from_secs(2));
    wait(cx, |_| !server.requests("POST", "/api/fs/write").is_empty());
    let world = server.world.lock().expect("world");
    assert!(
        !world.files.contains_key(&old),
        "autosave must not recreate a path moved by the late operation"
    );
    assert_eq!(world.files[&new].0, "retained late-rename draft");
}

#[gpui_kit::test]
fn an_explicit_same_target_rename_can_resolve_an_unmoved_source(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    let old = format!("{ROOT}/src/lib.txt");
    let new = format!("{ROOT}/src/moved.txt");
    load_file(cx, window, &view, &old);
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.toggle_folder(&format!("{ROOT}/src"), cx)
        })
    });
    wait_for_element(cx, window, &format!("files-row:{old}"));
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.check_rename(old.clone(), new.clone(), cx)
        })
    });
    wait(cx, |_| {
        server
            .requests("GET", "/api/fs/list")
            .iter()
            .filter(|request| request.param("path") == Some(&format!("{ROOT}/src")))
            .count()
            >= 2
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.start_rename(&old, window, cx));
        window.render_frame(cx);
        window.press("ctrl-a", cx);
        window.input("moved.txt", cx);
        window.press("enter", cx);
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| view.read(cx).uncertain_renames.is_empty())
    });
    let world = server.world.lock().expect("world");
    assert!(!world.files.contains_key(&old));
    assert!(world.files.contains_key(&new));
}

#[gpui_kit::test]
fn a_late_retry_response_cannot_resurrect_a_reconciled_rename(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    let old = format!("{ROOT}/src/lib.txt");
    let new = format!("{ROOT}/src/moved.txt");
    load_file(cx, window, &view, &old);
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.toggle_folder(&format!("{ROOT}/src"), cx)
        })
    });
    wait_for_element(cx, window, &format!("files-row:{old}"));
    edit_text(cx, window, &view, "retained rename draft");
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.check_rename(old.clone(), new.clone(), cx)
        })
    });
    server.held_rename_reply.store(true, Ordering::Release);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.start_rename(&old, window, cx));
        window.render_frame(cx);
        window.press("ctrl-a", cx);
        window.input("moved.txt", cx);
        window.press("enter", cx);
    })
    .expect("window");
    wait(cx, |_| {
        server.world.lock().expect("world").files.contains_key(&new)
    });
    let retry_revision = cx.read(|cx| view.read(cx).rename_revisions[&old]);
    // The retry committed, but its HTTP answer stays held while Refresh
    // observes the move and a second user rename moves it back.
    cx.update(|cx| view.update(cx, |view, cx| view.refresh_tree(cx)));
    wait(cx, |cx| {
        cx.read(|cx| view.read(cx).uncertain_renames.is_empty())
    });
    wait_for_element(cx, window, &format!("files-row:{new}"));
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.start_rename(&new, window, cx));
        window.render_frame(cx);
        window.press("ctrl-a", cx);
        window.input("lib.txt", cx);
        window.press("enter", cx);
    })
    .expect("window");
    wait(cx, |cx| cx.read(|cx| view.read(cx).renaming.is_empty()));
    server.held_rename_reply.store(false, Ordering::Release);
    wait(cx, |_| server.released_rename_reply.load(Ordering::Acquire));
    // Also exercise the completion boundary directly so this assertion does
    // not depend on the network executor's callback scheduling.
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.finish_rename(old.clone(), new.clone(), retry_revision, Some(Ok(())), cx);
            assert_eq!(view.open.as_ref().expect("editor").file.path(), old);
        })
    });
    assert_eq!(
        cx.read(|cx| editor_text(&view, cx)),
        Some("retained rename draft".into())
    );
    cx.executor().advance_clock(Duration::from_secs(2));
    wait(cx, |cx| {
        cx.read(|cx| !view.read(cx).open.as_ref().expect("editor").file.is_dirty())
    });
    let world = server.world.lock().expect("world");
    assert!(
        !world.files.contains_key(&new),
        "a stale completion must not recreate the old destination"
    );
    assert_eq!(world.files[&old].0, "retained rename draft");
    assert!(
        world
            .requests
            .iter()
            .filter(|request| request.path == "/api/fs/write")
            .all(|request| request.json()["path"] == old)
    );
}

#[gpui_kit::test]
fn hidden_dirty_editor_resumes_autosave_only_on_same_runtime_readiness(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (_, workspace) = crate::workspace_tests::open(cx);
    let config = crate::connection::ServerConfig {
        endpoint: server.endpoint.clone(),
        password: None,
        client: openchamber_native_transport::client::ClientDescription {
            label: "test".into(),
            kind: "desktop".into(),
            dedupe_key: None,
            device_platform: None,
            app_version: None,
        },
    };
    let mut connection = crate::connection::Connection::new(config).expect("network");
    connection.client = Some(harness.scope.client.clone());
    cx.update(|cx| workspace.update(cx, |workspace, _| workspace.connection = Some(connection)));
    let mut scope = harness.scope.clone();
    scope.owner = Some(workspace.downgrade());
    scope.runtime = cx.read(|cx| workspace.read(cx).runtime.clone());
    let runtime = scope.runtime.clone();
    let shown = std::rc::Rc::new(std::cell::Cell::new(true));
    let visibility = shown.clone();
    let (window, view) = open_view(cx, move |window, cx| {
        FilesView::new(scope, shown, window, cx)
    });
    load_file(cx, window, &view, &format!("{ROOT}/README.md"));
    edit_text(cx, window, &view, "offline draft");
    visibility.set(false);
    cx.update(|cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.connection.as_mut().expect("connection").client = None;
            cx.notify();
        })
    });
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(server.requests("POST", "/api/fs/write").is_empty());
    cx.update(|cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.runtime = RuntimeId::from("other");
            workspace.connection.as_mut().expect("connection").client =
                Some(harness.scope.client.clone());
            cx.notify();
        })
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(
        server.requests("POST", "/api/fs/write").is_empty(),
        "a different runtime cannot save"
    );
    cx.update(|cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.runtime = runtime;
            cx.notify();
        })
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(2));
    wait(cx, |_| !server.requests("POST", "/api/fs/write").is_empty());
    assert_eq!(
        server.world.lock().expect("world").files[&format!("{ROOT}/README.md")].0,
        "offline draft"
    );
}

#[gpui_kit::test]
fn operation_poll_stops_after_three_failures_and_refresh_retries_same_id(cx: &mut TestAppContext) {
    let server = Scripted::new(git_world());
    let harness = harness(&server);
    let (window, view) = open_git(cx, &harness);
    wait_for_element(cx, window, "git-stage:a.txt");
    cx.update(|cx| view.update(cx, |view, cx| view.start_test_operation(cx)));
    cx.run_until_parked();
    let route = "/api/git/network-operations/test-operation";
    wait(cx, |cx| {
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        cx.read(|cx| !view.read(cx).operation_polling())
    });
    cx.executor().advance_clock(Duration::from_secs(10));
    wait(cx, |cx| cx.read(|cx| !view.read(cx).operation_polling()));
    assert_eq!(server.requests("GET", route).len(), 3);
    cx.update(|cx| view.update(cx, |view, cx| view.refresh(cx)));
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(2));
    wait(cx, |_| server.requests("GET", route).len() == 4);
    cx.update(|cx| view.update(cx, |view, cx| view.cancel_operation(cx)));
    wait(cx, |_| {
        !server
            .requests("POST", &format!("{route}/cancel"))
            .is_empty()
    });
    wait(cx, |cx| {
        cx.read(|cx| view.read(cx).notice_text())
            .is_some_and(|notice| notice.contains("unscripted"))
    });
    assert!(
        server
            .requests("POST", "/api/git/network-operations")
            .is_empty(),
        "recovery never replans a push"
    );
}

#[gpui_kit::test]
fn cancelled_planning_clears_the_operation_and_reports_failure(cx: &mut TestAppContext) {
    let server = Scripted::new(git_world());
    let harness = harness(&server);
    let (window, view) = open_git(cx, &harness);
    wait_for_element(cx, window, "git-push");
    *server.held_route.lock().expect("gate") = Some("/api/source-control/binding".into());
    cx.update_window(window, |_, window, cx| window.click(name("git-push"), cx))
        .expect("window");
    wait(cx, |_| {
        !server
            .requests("GET", "/api/source-control/binding")
            .is_empty()
    });
    drop(harness);
    wait(cx, |cx| cx.read(|cx| view.read(cx).notice_text()).is_some());
    frame(cx, window);
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find(name("git-operation")).is_none())
    })
    .expect("window");
    *server.held_route.lock().expect("gate") = None;
}

#[gpui_kit::test]
fn discarding_mm_keeps_the_staged_row(cx: &mut TestAppContext) {
    let server = Scripted::new(git_world());
    let harness = harness(&server);
    let (window, _) = open_git(cx, &harness);
    wait_for_element(cx, window, "git-discard:a.txt");
    cx.update_window(window, |_, window, cx| {
        window.click(name("git-discard:a.txt"), cx)
    })
    .expect("window");
    wait_for_element(cx, window, "git-discard-confirm");
    cx.update_window(window, |_, window, cx| {
        window.click(name("git-discard-confirm"), cx)
    })
    .expect("window");
    wait(cx, |_| {
        server
            .world
            .lock()
            .expect("world")
            .status
            .iter()
            .any(|(path, index, working)| path == "a.txt" && *index == 'M' && *working == ' ')
    });
    wait_for_element(cx, window, "git-change:staged:a.txt");
    assert_eq!(
        server.requests("POST", "/api/git/revert")[0].json(),
        json!({"path": "a.txt", "scope": "working"})
    );
}

#[gpui_kit::test]
fn switching_during_an_existing_save_refuses_until_it_finishes(cx: &mut TestAppContext) {
    let server = Scripted::new(files_world());
    let harness = harness(&server);
    let (window, view) = open_files(cx, &harness);
    let path = format!("{ROOT}/README.md");
    load_file(cx, window, &view, &path);
    edit_text(cx, window, &view, "draft");
    *server.held_route.lock().expect("gate") = Some("/api/fs/write".into());
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.save(SaveTrigger::Explicit, cx);
        })
    });
    wait(cx, |_| !server.requests("POST", "/api/fs/write").is_empty());
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.open_file(&format!("{ROOT}/src/lib.txt"), window, cx);
            view.save_and_switch(cx);
        })
    })
    .expect("window");
    frame(cx, window);
    assert_eq!(
        cx.read(|cx| view
            .read(cx)
            .open
            .as_ref()
            .expect("open")
            .file
            .path()
            .to_owned()),
        path
    );
    assert!(cx.read(|cx| {
        view.read(cx)
            .switch
            .as_ref()
            .is_some_and(|switch| !switch.saving)
    }));
    *server.held_route.lock().expect("gate") = None;
    wait(cx, |cx| {
        cx.read(|cx| !view.read(cx).open.as_ref().expect("open").file.is_saving())
    });
    cx.update(|cx| view.update(cx, |view, cx| view.save_and_switch(cx)));
    wait(cx, |cx| {
        frame(cx, window);
        cx.read(|cx| editor_text(&view, cx)) == Some("lib\n".into())
    });
    assert_eq!(server.requests("POST", "/api/fs/write").len(), 1);
}

/// An element's left, top, width and height in logical pixels.
fn rect_of(window: &gpui_kit::Window, id: &str) -> [f32; 4] {
    let bounds = window.find(name(id)).bounds();
    [
        f32::from(bounds.left()),
        f32::from(bounds.top()),
        f32::from(bounds.size.width),
        f32::from(bounds.size.height),
    ]
}

/// Connects a 1280x800 workspace window, the parity runner's size, to
/// `server`.
fn open_connected(
    cx: &mut TestAppContext,
    server: &Scripted,
    harness: &Harness,
) -> (AnyWindowHandle, Entity<crate::workspace::Workspace>) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.executor().allow_parking();
    let config = crate::connection::ServerConfig {
        endpoint: server.endpoint.clone(),
        password: None,
        client: openchamber_native_transport::client::ClientDescription {
            label: "test".into(),
            kind: "desktop".into(),
            dedupe_key: None,
            device_platform: None,
            app_version: None,
        },
    };
    let mut connection = crate::connection::Connection::new(config).expect("network");
    connection.client = Some(harness.scope.client.clone());
    cx.update(|cx| workspace.update(cx, |workspace, _| workspace.connection = Some(connection)));
    (window, workspace)
}

/// Sidebar sessions in `count` different directories, with those
/// directories as the panel resolves them.
fn sessions_in_directories(
    workspace: &Entity<crate::workspace::Workspace>,
    cx: &mut TestAppContext,
    count: usize,
) -> Vec<(openchamber_native_core::ids::SessionId, String)> {
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        let mut found: Vec<(openchamber_native_core::ids::SessionId, String)> = Vec::new();
        for item in &workspace.sidebar_items {
            let crate::sidebar_item::SidebarItem::Session { session_id, .. } = item else {
                continue;
            };
            let directory = workspace
                .session(session_id)
                .and_then(|session| path::normalize_optional(session.directory.as_deref()));
            if let Some(directory) = directory
                && found.iter().all(|(_, known)| *known != directory)
            {
                found.push((session_id.clone(), directory));
            }
            if found.len() == count {
                break;
            }
        }
        assert_eq!(
            found.len(),
            count,
            "the synthetic workload has sessions in {count} directories"
        );
        found
    })
}

/// A project-less draft with nowhere to fall back to: its directory is
/// unresolved.
fn show_unresolved_draft(
    workspace: &Entity<crate::workspace::Workspace>,
    window: &mut gpui_kit::Window,
    cx: &mut gpui_kit::App,
) {
    workspace.update(cx, |workspace, cx| {
        workspace.show_new_session(window, cx);
        workspace.draft_projects.remove(&workspace.runtime.clone());
        workspace.send_catalog.settings = None;
        assert_eq!(workspace.panel_directory(), None);
    });
}

/// The Files zone at the parity runner's 1280x800 window, against the
/// packaged 2.6.1 geometry: the tree alone, a file open in a session, a
/// draft in another project, and the return to the session.
#[gpui_kit::test]
fn the_files_zone_matches_the_reference_at_the_runner_window(cx: &mut TestAppContext) {
    let server = Scripted::new(World::default());
    let harness = harness(&server);
    let (window, workspace) = open_connected(cx, &server, &harness);
    let (session, directory) = sessions_in_directories(&workspace, cx, 1).remove(0);
    // A registered project elsewhere, for a draft that leaves the session's
    // directory.
    let elsewhere = cx.read(|cx| {
        workspace
            .read(cx)
            .topology
            .projects
            .iter()
            .map(|project| project.path.clone())
            .find(|project| path::normalize(project).as_deref() != Some(directory.as_str()))
            .expect("the synthetic workload has a second project")
    });
    {
        let mut world = server.world.lock().expect("world");
        world.write(&format!("{directory}/README.md"), "# Readme\n");
        world.write(&format!("{directory}/src/app.txt"), "one\ntwo\n");
    }
    let folder = format!("files-row:{directory}/src");
    let readme = format!("files-row:{directory}/README.md");
    let file = format!("files-row:{directory}/src/app.txt");

    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.work_status_open = true;
            workspace.select_session(session.clone(), window, cx)
        });
        window.render_frame(cx);
        assert!(workspace.read(cx).sidebar_open);
        assert!(window.try_find(name("side-panel")).is_none());
        window.click(name("rail-files"), cx);
    })
    .expect("window is open");
    wait_for_element(cx, window, &readme);

    // The tree alone: the zone is the tree's 240 px, and the sidebar folds,
    // because the 956 px row would leave 316 px beside the stored 640 px.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(!workspace.read(cx).sidebar_open);
        assert_eq!(rect_of(window, "new-session"), [72.0, 12.0, 24.0, 24.0]);
        assert_eq!(rect_of(window, "side-panel"), [996.0, 48.0, 240.0, 752.0]);
        assert_eq!(rect_of(window, "files-panel"), [1004.0, 48.0, 226.0, 744.0]);
        // 1236 px of row keep the card's column beside the 240 px tree.
        let card = rect_of(window, "work-status");
        assert_eq!([card[0], card[1], card[2]], [680.0, 64.0, 300.0]);
        let tab = rect_of(window, "files-tab");
        assert_eq!([tab[0], tab[1], tab[3]], [1004.0, 48.0, 40.0]);
        assert!(window.try_find(name("files-toggle-editor")).is_none());
        assert_eq!(
            rect_of(window, "files-toggle-tree"),
            [1164.0, 54.0, 28.0, 28.0]
        );
        assert_eq!(
            rect_of(window, "files-close-panel"),
            [1196.0, 54.0, 28.0, 28.0]
        );
        for (id, left) in [
            ("files-show-gitignored", 1146.0),
            ("files-refresh", 1172.0),
            ("files-collapse-all", 1198.0),
        ] {
            let button = rect_of(window, id);
            assert_eq!(
                [button[0], button[2], button[3]],
                [left, 24.0, 24.0],
                "{id}"
            );
        }
        assert_eq!(
            rect_of(window, "files-search"),
            [1012.0, 129.0, 210.0, 32.0]
        );
        assert_eq!(rect_of(window, &folder), [1010.0, 176.0, 208.0, 26.0]);
        assert_eq!(rect_of(window, &readme), [1010.0, 202.0, 208.0, 26.0]);
        window.click(name(folder.clone()), cx);
    })
    .expect("window is open");
    wait_for_element(cx, window, &file);
    cx.update_window(window, |_, window, cx| window.click(name(file.clone()), cx))
        .expect("window is open");
    wait_for_element(cx, window, "files-save");

    // A file open in the session: the zone takes its stored 640 px, the
    // editor the room before the 240 px tree.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(!workspace.read(cx).sidebar_open);
        assert_eq!(rect_of(window, "new-session"), [72.0, 12.0, 24.0, 24.0]);
        assert_eq!(rect_of(window, "side-panel"), [596.0, 48.0, 640.0, 752.0]);
        assert_eq!(rect_of(window, "files-panel"), [604.0, 48.0, 626.0, 744.0]);
        assert!(
            window.try_find(name("work-status")).is_none(),
            "the card yields beside the 640 px zone"
        );
        let tab = rect_of(window, "files-tab");
        assert_eq!([tab[0], tab[1], tab[3]], [604.0, 48.0, 40.0]);
        assert_eq!(
            rect_of(window, "files-toggle-editor"),
            [1132.0, 54.0, 28.0, 28.0]
        );
        assert_eq!(
            rect_of(window, "files-toggle-tree"),
            [1164.0, 54.0, 28.0, 28.0]
        );
        assert_eq!(
            rect_of(window, "files-close-panel"),
            [1196.0, 54.0, 28.0, 28.0]
        );
        assert_eq!(
            rect_of(window, "files-editor"),
            [604.0, 121.0, 386.0, 671.0]
        );
        for (id, left) in [("files-preview", 932.0), ("files-save", 958.0)] {
            let button = rect_of(window, id);
            assert_eq!(
                [button[0], button[2], button[3]],
                [left, 24.0, 24.0],
                "{id}"
            );
        }
        assert_eq!(rect_of(window, "files-search"), [998.0, 129.0, 224.0, 32.0]);
        assert_eq!(rect_of(window, &folder), [996.0, 176.0, 222.0, 26.0]);
        assert_eq!(rect_of(window, &file), [996.0, 202.0, 222.0, 26.0]);
        assert_eq!(rect_of(window, &readme), [996.0, 228.0, 222.0, 26.0]);
        // A draft in another project, reached in one step.
        workspace.update(cx, |workspace, cx| {
            workspace.show_new_session(window, cx);
            workspace.choose_draft_project(&elsewhere, window, cx);
        });
    })
    .expect("window is open");

    // The draft's directory is another one: the session's zone stays behind
    // and the sidebar it folded comes back.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let state = workspace.read(cx);
        assert_eq!(state.selected, None);
        assert_eq!(state.panels.open, None);
        assert!(state.sidebar_open);
        assert!(window.try_find(name("side-panel")).is_none());
        let button = rect_of(window, "new-session");
        assert_eq!([button[0], button[1], button[3]], [72.0, 12.0, 24.0]);
        assert!(button[2] > 24.0, "the open sidebar labels New session");
        workspace.update(cx, |workspace, cx| {
            workspace.select_session(session.clone(), window, cx)
        });
        window.render_frame(cx);
        // The session's editor is back. A zone that is simply there after a
        // switch folds nothing, so it keeps the chat's 320 px by narrowing.
        let state = workspace.read(cx);
        assert_eq!(
            state.panels.open,
            Some(crate::files_view::PanelSurface::Files)
        );
        assert!(state.sidebar_open);
        assert_eq!(rect_of(window, "side-panel"), [600.0, 48.0, 636.0, 752.0]);
        assert!(window.try_find(name("files-editor")).is_some());
        window.click(name("files-close-panel"), cx);
    })
    .expect("window is open");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(workspace.read(cx).panels.open, None);
        assert!(window.try_find(name("side-panel")).is_none());
        // Opened again with the sidebar open, the zone folds it; closing the
        // zone gives it back.
        window.click(name("rail-files"), cx);
        window.render_frame(cx);
        assert!(!workspace.read(cx).sidebar_open);
        window.click(name("rail-files"), cx);
        window.render_frame(cx);
        assert!(workspace.read(cx).sidebar_open);
        // The user's own toggle is theirs to keep.
        window.click(name("rail-files"), cx);
        window.render_frame(cx);
        window.click(name("sidebar-toggle"), cx);
        window.render_frame(cx);
        assert!(workspace.read(cx).sidebar_open);
        window.click(name("sidebar-toggle"), cx);
        window.render_frame(cx);
        window.click(name("rail-files"), cx);
        window.render_frame(cx);
        assert!(
            !workspace.read(cx).sidebar_open,
            "a sidebar the user closed stays closed when the zone closes"
        );
    })
    .expect("window is open");
}

/// An unresolved directory neither owns nor receives zone state: the zone on
/// screen stays through it and goes to the first resolved directory.
#[gpui_kit::test]
fn an_unresolved_directory_carries_the_zone_to_the_next_resolved_one(cx: &mut TestAppContext) {
    use crate::files_view::PanelSurface;

    let (window, workspace) = crate::workspace_tests::open(cx);
    let sessions = sessions_in_directories(&workspace, cx, 2);
    let (first, second) = (sessions[0].0.clone(), sessions[1].0.clone());
    let open = |workspace: &Entity<crate::workspace::Workspace>, cx: &gpui_kit::App| {
        workspace.read(cx).panels.open
    };
    cx.update_window(window, |_, window, cx| {
        // Unresolved to resolved: a zone opened before any directory is
        // known is not closed by the first one that resolves.
        show_unresolved_draft(&workspace, window, cx);
        window.render_frame(cx);
        window.click(name("rail-git"), cx);
        window.render_frame(cx);
        assert_eq!(open(&workspace, cx), Some(PanelSurface::Git));
        workspace.update(cx, |workspace, cx| {
            workspace.select_session(first.clone(), window, cx)
        });
        window.render_frame(cx);
        assert_eq!(open(&workspace, cx), Some(PanelSurface::Git));
        assert!(window.try_find(name("side-panel")).is_some());

        // Resolved to unresolved and back: nothing is parked or restored.
        show_unresolved_draft(&workspace, window, cx);
        window.render_frame(cx);
        assert_eq!(open(&workspace, cx), Some(PanelSurface::Git));
        workspace.update(cx, |workspace, cx| {
            workspace.select_session(first.clone(), window, cx)
        });
        window.render_frame(cx);
        assert_eq!(open(&workspace, cx), Some(PanelSurface::Git));

        // Through an unresolved frame to another directory: the zone is
        // carried there, and the first directory kept nothing.
        show_unresolved_draft(&workspace, window, cx);
        window.render_frame(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.select_session(second.clone(), window, cx)
        });
        window.render_frame(cx);
        assert_eq!(open(&workspace, cx), Some(PanelSurface::Git));
        workspace.update(cx, |workspace, cx| {
            workspace.select_session(first.clone(), window, cx)
        });
        window.render_frame(cx);
        assert_eq!(
            open(&workspace, cx),
            None,
            "resolved to resolved parks the zone"
        );
        workspace.update(cx, |workspace, cx| {
            workspace.select_session(second.clone(), window, cx)
        });
        window.render_frame(cx);
        assert_eq!(open(&workspace, cx), Some(PanelSurface::Git));
    })
    .expect("window is open");
}

/// A parked extension panel returns as itself, whatever extension another
/// directory opened meanwhile.
#[gpui_kit::test]
fn a_parked_extension_panel_returns_as_itself(cx: &mut TestAppContext) {
    use crate::files_view::PanelSurface;
    use openchamber_native_transport::web_content::GuestPanel;

    let (window, workspace) = crate::workspace_tests::open(cx);
    let sessions = sessions_in_directories(&workspace, cx, 2);
    let (first, second) = (sessions[0].0.clone(), sessions[1].0.clone());
    let guest = |id: &str| GuestPanel {
        id: id.to_owned(),
        name: id.to_owned(),
        icon: "apps".to_owned(),
        entry: "index.html".to_owned(),
    };
    let shown = |workspace: &Entity<crate::workspace::Workspace>, cx: &gpui_kit::App| {
        let workspace = workspace.read(cx);
        (
            workspace.panels.open,
            workspace
                .panels
                .open_guest
                .as_ref()
                .map(|panel| panel.id.clone()),
        )
    };
    cx.update_window(window, |_, window, cx| {
        for (session, id) in [(&first, "alpha"), (&second, "beta")] {
            workspace.update(cx, |workspace, cx| {
                workspace.select_session(session.clone(), window, cx)
            });
            window.render_frame(cx);
            assert_eq!(shown(&workspace, cx).0, None);
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_guest_panel(guest(id), cx)
            });
            window.render_frame(cx);
        }
        for (session, id) in [(&first, "alpha"), (&second, "beta"), (&first, "alpha")] {
            workspace.update(cx, |workspace, cx| {
                workspace.select_session(session.clone(), window, cx)
            });
            window.render_frame(cx);
            assert_eq!(
                shown(&workspace, cx),
                (Some(PanelSurface::Guest), Some(id.to_owned()))
            );
        }
    })
    .expect("window is open");
}

/// The tree toggle is the user's: frames with no file open draw the tree
/// without resetting it, and only opening Files with no file does.
#[gpui_kit::test]
fn the_hidden_tree_toggle_is_reset_only_when_files_opens_with_no_file(cx: &mut TestAppContext) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    let (session, _) = sessions_in_directories(&workspace, cx, 1).remove(0);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.select_session(session.clone(), window, cx)
        });
        window.render_frame(cx);
        window.click(name("rail-files"), cx);
        window.render_frame(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.pane_sizes.tree_hidden = true;
            cx.notify();
        });
        for _ in 0..3 {
            window.render_frame(cx);
            let state = workspace.read(cx);
            assert!(state.pane_sizes.tree_hidden, "a frame leaves the toggle");
            assert!(state.panels.tree_only, "with no file the tree is drawn");
        }
        assert_eq!(rect_of(window, "side-panel")[2], 240.0);
        // Another surface and back to Files: Files opens with no file.
        window.click(name("rail-git"), cx);
        window.render_frame(cx);
        assert!(workspace.read(cx).pane_sizes.tree_hidden);
        window.click(name("rail-files"), cx);
        window.render_frame(cx);
        assert!(!workspace.read(cx).pane_sizes.tree_hidden);
    })
    .expect("window is open");
}

/// With the editor hidden, the open file's row shows the editor again, and a
/// row that raises the unsaved-changes dialog gives the dialog the editor's
/// room.
#[gpui_kit::test]
fn a_file_row_shows_the_hidden_editor_and_fits_the_unsaved_dialog(cx: &mut TestAppContext) {
    let server = Scripted::new(World::default());
    let harness = harness(&server);
    let (window, workspace) = open_connected(cx, &server, &harness);
    let (session, directory) = sessions_in_directories(&workspace, cx, 1).remove(0);
    {
        let mut world = server.world.lock().expect("world");
        world.write(&format!("{directory}/README.md"), "# Readme\n");
        world.write(&format!("{directory}/notes.txt"), "notes\n");
    }
    let readme = format!("files-row:{directory}/README.md");
    let notes = format!("files-row:{directory}/notes.txt");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.select_session(session.clone(), window, cx)
        });
        window.render_frame(cx);
        window.click(name("rail-files"), cx);
    })
    .expect("window is open");
    wait_for_element(cx, window, &readme);
    cx.update_window(window, |_, window, cx| {
        window.click(name(readme.clone()), cx)
    })
    .expect("window is open");
    wait_for_element(cx, window, "files-save");
    let hide_editor = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(name("files-toggle-editor"), cx);
        })
        .expect("window is open");
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(workspace.read(cx).pane_sizes.editor_hidden);
            assert_eq!(rect_of(window, "side-panel")[2], 240.0);
            assert!(window.try_find(name("files-editor")).is_none());
        })
        .expect("window is open");
    };

    // The open file's own row.
    hide_editor(cx);
    cx.update_window(window, |_, window, cx| {
        window.click(name(readme.clone()), cx)
    })
    .expect("window is open");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(!workspace.read(cx).pane_sizes.editor_hidden);
        assert_eq!(rect_of(window, "side-panel")[2], 640.0);
        assert!(window.try_find(name("files-editor")).is_some());
        // An unsaved edit, then another file's row with the editor hidden.
        window.click(name("files-editor"), cx);
        window.press("ctrl-end", cx);
        window.input("kept", cx);
    })
    .expect("window is open");
    cx.run_until_parked();
    hide_editor(cx);
    cx.update_window(window, |_, window, cx| {
        window.click(name(notes.clone()), cx)
    })
    .expect("window is open");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(!workspace.read(cx).pane_sizes.editor_hidden);
        let card = rect_of(window, "files-panel");
        assert_eq!(card, [604.0, 48.0, 626.0, 744.0]);
        // 320 px wide, centred in the card: nothing of it is clipped.
        let dialog = rect_of(window, "files-unsaved");
        assert_eq!([dialog[0], dialog[2]], [757.0, 320.0]);
        assert!(dialog[1] >= card[1] && dialog[1] + dialog[3] <= card[1] + card[3]);
        window.click(name("files-unsaved-cancel"), cx);
    })
    .expect("window is open");
    assert!(
        server.requests("POST", "/api/fs/write").is_empty(),
        "cancelling keeps the draft unwritten"
    );
}

/// A draft with no project is not a chat draft: its surfaces keep working
/// in the active project, where a file is saved and a change staged.
#[gpui_kit::test]
fn a_project_less_draft_saves_and_stages_in_the_active_project(cx: &mut TestAppContext) {
    let mut world = files_world();
    world.status = vec![("new.txt".to_owned(), '?', '?')];
    let server = Scripted::new(world);
    let harness = harness(&server);
    let (window, workspace) = open_connected(cx, &server, &harness);
    let readme = format!("{ROOT}/README.md");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.show_new_session(window, cx);
            workspace.draft_projects.remove(&workspace.runtime.clone());
            workspace.send_catalog.settings =
                Some(openchamber_native_transport::client::ServerSettings {
                    projects: vec![openchamber_native_core::ownership::ConfiguredProject {
                        id: "active".into(),
                        path: ROOT.to_owned(),
                    }],
                    active_project_id: Some("active".to_owned()),
                    ..Default::default()
                });
            assert!(
                workspace.topology.chat_roots.is_some(),
                "the Chats root is known and still not used"
            );
            assert_eq!(workspace.panel_directory().as_deref(), Some(ROOT));
            assert_eq!(workspace.terminal_directory().as_deref(), Some(ROOT));
        });
        window.render_frame(cx);
        window.click(name("rail-files"), cx);
    })
    .expect("window is open");
    wait_for_element(cx, window, &format!("files-row:{readme}"));
    cx.update_window(window, |_, window, cx| {
        window.click(name(format!("files-row:{readme}")), cx)
    })
    .expect("window is open");
    wait_for_element(cx, window, "files-save");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(name("files-editor"), cx);
        window.press("ctrl-end", cx);
        window.input("more", cx);
    })
    .expect("window is open");
    // The editor's change reaches the view once the input's update ends.
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(name("files-save"), cx);
    })
    .expect("window is open");
    wait(cx, |_| !server.requests("POST", "/api/fs/write").is_empty());
    let write = server.requests("POST", "/api/fs/write").remove(0);
    // The write's directory travels in a header the scripted server does
    // not keep; its path is inside the active project.
    assert_eq!(write.json()["path"], json!(readme));

    cx.update_window(window, |_, window, cx| window.click(name("rail-git"), cx))
        .expect("window is open");
    wait_for_element(cx, window, "git-stage:new.txt");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(name("git-stage:new.txt"), cx);
    })
    .expect("window is open");
    wait(cx, |_| {
        !server.requests("POST", "/api/git/stage").is_empty()
    });
    let stage = server.requests("POST", "/api/git/stage").remove(0);
    assert_eq!(stage.param("directory"), Some(ROOT));
    assert!(
        server
            .requests("GET", "/api/git/status")
            .iter()
            .all(|status| status.param("directory") == Some(ROOT)),
        "every status read names the active project"
    );
}

fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<gpui_kit::Pixels> {
    Bounds {
        origin: point(px(x), px(y)),
        size: size(px(width), px(height)),
    }
}

/// `bounds` with its top measured from `top`.
fn below(bounds: Bounds<gpui_kit::Pixels>, top: gpui_kit::Pixels) -> Bounds<gpui_kit::Pixels> {
    Bounds {
        origin: point(bounds.left(), bounds.top() - top),
        size: bounds.size,
    }
}

#[gpui_kit::test]
fn the_git_panel_lays_out_its_groups_rows_and_commit_section_as_the_reference_does(
    cx: &mut TestAppContext,
) {
    let server = Scripted::new(git_world());
    let harness = harness(&server);
    let (window, view) = open_git(cx, &harness);
    wait_for_element(cx, window, "git-change:unstaged:new.txt");
    cx.run_until_parked();
    let quiet = cx.read(|cx| view.read(cx).notice_text().is_none());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        // The header row with its line, then who commits.
        assert_eq!(
            window.find("git-header").bounds(),
            rect(0.0, 0.0, 640.0, 41.0)
        );
        assert_eq!(
            window.find("git-branch-button").bounds().origin,
            point(px(6.0), px(6.0))
        );
        assert_eq!(
            window.find("git-state-row").bounds(),
            rect(0.0, 41.0, 640.0, 40.0)
        );
        assert_eq!(
            window.find("git-identity").bounds(),
            rect(12.0, 47.0, 54.0, 28.0)
        );

        // The list stands in `px-4 pt-1`; its rows stop 8 px short of its end.
        let list = window.find("git-changes").bounds();
        assert_eq!(list.left(), px(16.0));
        assert_eq!(list.size.width, px(608.0));
        if quiet {
            assert_eq!(
                list.top(),
                px(85.0),
                "no outcome line stands above the list"
            );
        }
        let top = list.top();
        let expected = [
            ("git-unstage-all", rect(16.0, 8.0, 20.0, 20.0)),
            ("git-group:staged", rect(44.0, 8.0, 572.0, 20.0)),
            ("git-unstage:a.txt", rect(16.0, 42.0, 20.0, 20.0)),
            ("git-change:staged:a.txt", rect(44.0, 36.0, 572.0, 32.0)),
            // The second group: 4 px, its line, then the same 36 px.
            ("git-stage-all", rect(16.0, 81.0, 20.0, 20.0)),
            ("git-group:unstaged", rect(44.0, 81.0, 572.0, 20.0)),
            ("git-stage:a.txt", rect(16.0, 115.0, 20.0, 20.0)),
            ("git-change:unstaged:a.txt", rect(44.0, 109.0, 544.0, 32.0)),
            ("git-discard:a.txt", rect(596.0, 115.0, 20.0, 20.0)),
            ("git-change:unstaged:b.txt", rect(44.0, 141.0, 544.0, 32.0)),
            (
                "git-change:unstaged:new.txt",
                rect(44.0, 173.0, 544.0, 32.0),
            ),
            ("git-discard:new.txt", rect(596.0, 179.0, 20.0, 20.0)),
        ];
        for (id, bounds) in expected {
            assert_eq!(below(window.find(name(id)).bounds(), top), bounds, "{id}");
        }
        let revert_all = below(window.find("git-revert-all").bounds(), top);
        assert_eq!(revert_all.top(), px(213.0));
        assert_eq!(revert_all.size.height, px(28.0));
        assert_eq!(revert_all.right(), px(616.0));

        // The commit section ends 16 px above the panel's bottom.
        assert_eq!(
            window.find("git-commit-message").bounds(),
            rect(16.0, 594.0, 608.0, 38.0)
        );
        let commit = window.find("git-commit").bounds();
        assert_eq!(commit.top(), px(644.0));
        assert_eq!(commit.size.height, px(28.0));
        assert_eq!(commit.right(), px(624.0));

        window.click(name("git-group:unstaged"), cx);
        window.render_frame(cx);
        assert!(
            window.try_find(name("git-change:unstaged:b.txt")).is_none(),
            "a collapsed group shows its header only"
        );
        assert!(window.try_find(name("git-change:staged:a.txt")).is_some());
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn the_changes_panel_lists_collapsed_files_and_opens_a_patch_under_its_row(
    cx: &mut TestAppContext,
) {
    let server = Scripted::new(git_world());
    let harness = harness(&server);
    let scope = harness.scope.clone();
    let (window, _view) = open_view(cx, move |_, cx| DiffView::new(scope, cx));
    wait_for_element(cx, window, "diff-file:new.txt");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("diff-toolbar").bounds(),
            rect(0.0, 0.0, 640.0, 41.0)
        );
        let scope = window.find("diff-scope").bounds();
        assert_eq!(scope.origin, point(px(6.0), px(6.0)));
        assert_eq!(scope.size.height, px(28.0));
        assert_eq!(
            window.find("diff-split").bounds(),
            rect(610.0, 8.0, 24.0, 24.0)
        );
        assert_eq!(
            window.find("diff-wrap").bounds(),
            rect(578.0, 8.0, 24.0, 24.0)
        );
        assert_eq!(
            window.find("diff-walkthrough").bounds(),
            rect(542.0, 6.0, 28.0, 28.0)
        );
        let expand = window.find("diff-expand-all").bounds();
        assert_eq!(expand.right(), px(534.0));
        assert_eq!(expand.top(), px(6.0));
        assert_eq!(expand.size.height, px(28.0));

        // Each file: a 40 px row, its line, then the file's own line.
        assert_eq!(
            window.find("diff-content").bounds(),
            rect(0.0, 41.0, 640.0, 659.0)
        );
        assert_eq!(
            window.find(name("diff-file:a.txt")).bounds(),
            rect(0.0, 41.0, 640.0, 40.0)
        );
        assert_eq!(
            window.find(name("diff-file:b.txt")).bounds(),
            rect(0.0, 83.0, 640.0, 40.0)
        );
        assert_eq!(
            window.find(name("diff-file:new.txt")).bounds(),
            rect(0.0, 125.0, 640.0, 40.0)
        );
        assert!(
            window.try_find(name("diff-file-patch:b.txt")).is_none(),
            "files start collapsed"
        );
        window.click(name("diff-file:b.txt"), cx);
    })
    .expect("window is open");
    wait_for_element(cx, window, "diff-file-patch:b.txt");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let patch = window.find(name("diff-file-patch:b.txt")).bounds();
        assert_eq!(patch.origin, point(px(0.0), px(124.0)));
        assert_eq!(patch.size.width, px(640.0));
        assert_eq!(
            window.find(name("diff-file:new.txt")).bounds().top(),
            patch.bottom() + px(1.0),
            "the next file follows the open patch and the file's line"
        );
        assert_eq!(
            window.find(name("diff-file:a.txt")).bounds(),
            rect(0.0, 41.0, 640.0, 40.0),
            "opening a file moves nothing above it"
        );
    })
    .expect("window is open");
}

/// Opens the Revert all dialog, then presses its confirm button's action.
fn ask_revert_all(cx: &mut TestAppContext, window: AnyWindowHandle, view: &Entity<GitView>) {
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.request_revert_all(window, cx))
    })
    .expect("window is open");
}

fn confirm_revert_all(cx: &mut TestAppContext, window: AnyWindowHandle, view: &Entity<GitView>) {
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.confirm_revert_all(window, cx))
    })
    .expect("window is open");
}

fn reverted_paths(server: &Scripted) -> Vec<(String, String)> {
    server
        .requests("POST", "/api/git/revert")
        .iter()
        .map(|request| {
            let body = request.json();
            (
                body["path"].as_str().unwrap_or_default().to_owned(),
                body["scope"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[gpui_kit::test]
fn revert_all_reports_one_summary_that_keeps_an_earlier_failure(cx: &mut TestAppContext) {
    let server = Scripted::new(World {
        revert_fail: vec!["b.txt".to_owned()],
        ..git_world()
    });
    let harness = harness(&server);
    let (window, view) = open_git(cx, &harness);
    wait_for_element(cx, window, "git-change:unstaged:new.txt");
    cx.run_until_parked();
    ask_revert_all(cx, window, &view);
    assert_eq!(
        cx.read(|cx| view.read(cx).revert_all_asking()),
        Some(3),
        "the dialog names every path of both groups once"
    );
    let reads = server.requests("GET", "/api/git/status").len();
    confirm_revert_all(cx, window, &view);
    wait(cx, |cx| {
        cx.read(|cx| !view.read(cx).revert_all_running())
            && server.requests("GET", "/api/git/status").len() > reads
    });
    cx.run_until_parked();
    // b.txt failed between two successes; the summary still says so.
    assert_eq!(
        cx.read(|cx| view.read(cx).notice_text()).as_deref(),
        Some("Reverted 2 files, 1 failed")
    );
    assert_eq!(
        reverted_paths(&server),
        [
            ("a.txt".to_owned(), "all".to_owned()),
            ("b.txt".to_owned(), "all".to_owned()),
            ("new.txt".to_owned(), "all".to_owned()),
        ]
    );
    assert_eq!(
        server.requests("GET", "/api/git/status").len(),
        reads + 1,
        "the batch reads the status once"
    );
}

#[gpui_kit::test]
fn revert_all_runs_once_at_a_time_and_leaves_queued_paths_alone(cx: &mut TestAppContext) {
    let server = Scripted::new(git_world());
    let harness = harness(&server);
    let (window, view) = open_git(cx, &harness);
    wait_for_element(cx, window, "git-stage:new.txt");
    cx.run_until_parked();
    // new.txt is being staged when the batch starts: the batch leaves it.
    *server.held_route.lock().expect("gate") = Some("/api/git/stage".into());
    cx.update_window(window, |_, window, cx| {
        window.click(name("git-stage:new.txt"), cx)
    })
    .expect("window is open");
    wait(cx, |_| {
        !server.requests("POST", "/api/git/stage").is_empty()
    });
    ask_revert_all(cx, window, &view);
    confirm_revert_all(cx, window, &view);
    assert!(cx.read(|cx| view.read(cx).revert_all_running()));
    // A second request while the batch runs opens nothing.
    ask_revert_all(cx, window, &view);
    assert_eq!(cx.read(|cx| view.read(cx).revert_all_asking()), None);
    *server.held_route.lock().expect("gate") = None;
    wait(cx, |cx| cx.read(|cx| !view.read(cx).revert_all_running()));
    assert_eq!(
        reverted_paths(&server),
        [
            ("a.txt".to_owned(), "all".to_owned()),
            ("b.txt".to_owned(), "all".to_owned()),
        ]
    );
}

#[gpui_kit::test]
fn revert_all_asks_again_when_the_listed_paths_changed(cx: &mut TestAppContext) {
    let server = Scripted::new(git_world());
    let harness = harness(&server);
    let (window, view) = open_git(cx, &harness);
    wait_for_element(cx, window, "git-change:unstaged:new.txt");
    cx.run_until_parked();
    ask_revert_all(cx, window, &view);
    assert_eq!(cx.read(|cx| view.read(cx).revert_all_asking()), Some(3));
    // The list changes under the open dialog.
    server
        .world
        .lock()
        .expect("world")
        .status
        .retain(|(path, _, _)| path != "new.txt");
    cx.update(|cx| view.update(cx, |view, cx| view.refresh(cx)));
    wait(cx, |cx| {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window
                .try_find(name("git-change:unstaged:new.txt"))
                .is_none()
        })
        .expect("window is open")
    });
    confirm_revert_all(cx, window, &view);
    cx.run_until_parked();
    assert!(
        reverted_paths(&server).is_empty(),
        "a confirmation for an older list reverts nothing"
    );
    assert_eq!(
        cx.read(|cx| view.read(cx).revert_all_asking()),
        Some(2),
        "the dialog asks again for what is listed now"
    );
    confirm_revert_all(cx, window, &view);
    wait(cx, |cx| cx.read(|cx| !view.read(cx).revert_all_running()));
    assert_eq!(reverted_paths(&server).len(), 2);
}

#[gpui_kit::test]
fn a_patch_that_does_not_parse_and_an_omitted_path_show_failures(cx: &mut TestAppContext) {
    let server = Scripted::new(World {
        bad_patch: vec!["b.txt".to_owned()],
        omit_patch: vec!["new.txt".to_owned()],
        ..git_world()
    });
    let harness = harness(&server);
    let scope = harness.scope.clone();
    let (window, view) = open_view(cx, move |_, cx| DiffView::new(scope, cx));
    wait_for_element(cx, window, "diff-file:new.txt");
    assert_eq!(
        cx.read(|cx| view.read(cx).failure_of("new.txt")),
        Some(String::new()),
        "a path the server left out is a failure"
    );
    cx.update_window(window, |_, window, cx| {
        window.click(name("diff-file:b.txt"), cx);
        window.click(name("diff-file:a.txt"), cx);
    })
    .expect("window is open");
    wait_for_element(cx, window, "diff-file-message:b.txt");
    wait_for_element(cx, window, "diff-file-patch:a.txt");
    let failure = cx.read(|cx| view.read(cx).failure_of("b.txt"));
    assert!(
        failure.as_deref().is_some_and(|detail| !detail.is_empty()),
        "the parse error keeps its message: {failure:?}"
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(name("diff-file-patch:b.txt")).is_none());
    })
    .expect("window is open");
    // Only the opened, parsed file holds a Kit state.
    assert!(cx.read(|cx| view.read(cx).has_state("a.txt")));
    assert!(!cx.read(|cx| view.read(cx).has_state("new.txt")));
}

#[gpui_kit::test]
fn a_read_started_for_the_other_scope_never_fills_the_new_one(cx: &mut TestAppContext) {
    let server = Scripted::new(git_world());
    let harness = harness(&server);
    let scope = harness.scope.clone();
    *server.held_route.lock().expect("gate") = Some("/api/git/diffs".into());
    let (window, view) = open_view(cx, move |_, cx| DiffView::new(scope, cx));
    // The Changed read waits on its patches when the scope changes.
    wait(cx, |_| {
        !server.requests("POST", "/api/git/diffs").is_empty()
    });
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.set_scope(
                openchamber_native_core::git_changes::ChangeScope::Staged,
                cx,
            )
        })
    });
    *server.held_route.lock().expect("gate") = None;
    wait_for_element(cx, window, "diff-file:a.txt");
    cx.run_until_parked();
    assert_eq!(
        cx.read(|cx| view.read(cx).listed_paths()),
        ["a.txt".to_owned()],
        "only the staged path is listed under Staged"
    );
    assert_eq!(
        cx.read(|cx| view.read(cx).scope_shown()),
        openchamber_native_core::git_changes::ChangeScope::Staged
    );
}
