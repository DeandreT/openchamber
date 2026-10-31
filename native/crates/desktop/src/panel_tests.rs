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
