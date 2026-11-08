//! A loopback OpenChamber server for Settings page tests: a route function
//! answers each request, and every request is recorded.

use std::io::{Read, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use openchamber_native_transport::client::{ServerClient, ServerSettings};
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use serde_json::Value;

use crate::connection::{Connection, ServerConfig};
use crate::settings_navigation::Page;
use crate::workspace::Workspace;

/// One received request: method, path with query, and JSON body.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Request {
    pub method: String,
    pub target: String,
    pub body: Value,
}

impl Request {
    pub fn is(&self, method: &str, path: &str) -> bool {
        self.method == method && self.target.split('?').next() == Some(path)
    }
}

type Route = dyn Fn(&Request) -> (u16, Value) + Send + Sync;

pub(crate) struct Server {
    pub endpoint: Endpoint,
    address: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Server {
    pub fn new(route: impl Fn(&Request) -> (u16, Value) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("owned loopback listener");
        let address = listener.local_addr().expect("listener address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (recorded, stopped) = (Arc::clone(&requests), Arc::clone(&stop));
        let route: Arc<Route> = Arc::new(route);
        let worker = thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                let (status, body) = route(&request);
                recorded.lock().expect("requests").push(request);
                let body = body.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            endpoint: Endpoint::parse(&format!("http://{address}")).expect("endpoint"),
            address,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("requests").clone()
    }

    /// Requests other than reads.
    pub fn writes(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| request.method != "GET")
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).ok()?;
        headers.push(byte[0]);
    }
    let headers = String::from_utf8(headers).ok()?;
    let mut line = headers.lines().next()?.split_whitespace();
    let method = line.next()?.to_owned();
    let target = line.next()?.to_owned();
    let length: usize = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).ok()?;
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).ok()?
    };
    Some(Request {
        method,
        target,
        body,
    })
}

/// A workspace connected to `server`, with Settings open on `page`.
pub(crate) fn open(
    server: &Server,
    page: Page,
    cx: &mut TestAppContext,
) -> (AnyWindowHandle, Entity<Workspace>) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let mut connection =
                Connection::new(ServerConfig::for_endpoint(server.endpoint.clone()))
                    .expect("network");
            connection.client = Some(
                ServerClient::new(server.endpoint.clone(), Credential::None)
                    .expect("fixture client"),
            );
            workspace.connection = Some(connection);
            workspace.send_catalog.settings = Some(ServerSettings::default());
            workspace.show_settings(Some(page), window, cx);
        });
    })
    .expect("test window");
    settle(window, cx);
    (window, workspace)
}

/// Lets loopback requests finish and their completions render.
pub(crate) fn settle(window: AnyWindowHandle, cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    for _ in 0..20 {
        cx.run_until_parked();
        thread::sleep(Duration::from_millis(10));
    }
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .expect("test window");
}

/// The open Settings dialog's model.
pub(crate) fn settings(
    workspace: &Entity<Workspace>,
    cx: &TestAppContext,
) -> Entity<crate::settings_view::Settings> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .settings
            .clone()
            .expect("settings dialog")
    })
}
