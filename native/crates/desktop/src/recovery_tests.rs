//! Headless recovery tests with real journal workers and loopback HTTP requests.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext, WindowOptions};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_core::send_config::{
    AgentEntry, CatalogModel, CatalogProvider, ModelChoice,
};
use openchamber_native_core::session::SessionRecord;
use openchamber_native_core::synthetic::WorkloadSize;
use openchamber_native_transport::client::{ClientDescription, ServerClient};
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use serde_json::Value;

use crate::connection::{Connection, ConnectionState, ServerConfig};
use crate::locale::Strings;
use crate::mutation_journal::{JournalCommit, JournalSnapshot, LoadedJournal, MutationJournal};
use crate::send_attempt::{Admission, SessionKey, tests};
use crate::send_catalog::{DirectoryCatalog, Load};
use crate::theme::{self, Appearance, Palette};
use crate::workspace::{Source, Workspace};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

#[path = "creation_tests.rs"]
mod creation_tests;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!(
            "/tmp/opencode/native-recovery-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("unique test directory is created");
        Self(path)
    }

    fn journal(&self) -> PathBuf {
        self.0.join("profile").join("drafts-and-sends.json")
    }

    fn seed(&self, key: &SessionKey) {
        let mut attempt = tests::attempt(key);
        attempt.admission = Admission::Uncertain;
        let (mut journal, _) = MutationJournal::open(&self.journal()).expect("journal opens");
        let result = journal
            .save(&JournalSnapshot {
                drafts: Vec::new(),
                attempts: vec![(key.clone(), attempt)],
                creations: Vec::new(),
            })
            .expect("attempt saves");
        assert!(matches!(result, JournalCommit::Durable));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
    journal: Option<Value>,
}

struct Server {
    endpoint: Endpoint,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    release_prompt: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn new(journal: PathBuf, hold_prompt: bool) -> Self {
        Self::with_delivered(journal, hold_prompt, None)
    }

    fn with_delivered(
        journal: PathBuf,
        hold_prompt: bool,
        delivered: Option<(String, String)>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener binds");
        listener
            .set_nonblocking(true)
            .expect("listener is nonblocking");
        let endpoint = Endpoint::parse(&format!(
            "http://{}",
            listener.local_addr().expect("listener has an address")
        ))
        .expect("loopback endpoint parses");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let release_prompt = Arc::new(AtomicBool::new(!hold_prompt));
        let recorded = requests.clone();
        let stopped = stop.clone();
        let released = release_prompt.clone();
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
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .expect("write timeout");
                let mut request = read_request(&mut stream);
                if request.method == "POST" {
                    request.journal = fs::read(&journal)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
                }
                let prompt = request.path.ends_with("/prompt");
                let (status, body) = if request.method == "POST" && !prompt {
                    ("204 No Content", "")
                } else if prompt {
                    // A malformed success response leaves admission unknown.
                    ("200 OK", "{}")
                } else if request.path.ends_with("/inbox") {
                    ("200 OK", "{\"data\":[]}")
                } else if let Some((path, body)) = &delivered
                    && request.path == *path
                {
                    ("200 OK", body.as_str())
                } else {
                    ("404 Not Found", "{}")
                };
                recorded.lock().expect("request lock").push(request);
                if prompt {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !released.load(Ordering::Acquire) && !stopped.load(Ordering::Acquire) {
                        assert!(Instant::now() < deadline, "test releases the held response");
                        thread::sleep(Duration::from_millis(1));
                    }
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
            stop,
            release_prompt,
            worker: Some(worker),
        }
    }

    fn prompt_count(&self) -> usize {
        self.requests
            .lock()
            .expect("request lock")
            .iter()
            .filter(|request| request.method == "POST" && request.path.ends_with("/prompt"))
            .count()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.release_prompt.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.expect("loopback server completes");
            }
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut byte = [0];
        stream
            .read_exact(&mut byte)
            .expect("request headers arrive");
        bytes.push(byte[0]);
        assert!(bytes.len() < 65536, "fixture request headers are bounded");
        if bytes.ends_with(b"\r\n\r\n") {
            break bytes.len();
        }
    };
    let headers = String::from_utf8(bytes).expect("HTTP headers are ASCII");
    let mut first = headers
        .lines()
        .next()
        .expect("request line")
        .split_whitespace();
    let method = first.next().expect("method").to_owned();
    let path = first.next().expect("path").to_owned();
    let length: usize = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| {
            value.trim().parse().expect("content length")
        });
    assert!(
        header_end + length < 1024 * 1024,
        "fixture request is bounded"
    );
    let mut body = vec![0; length];
    stream.read_exact(&mut body).expect("request body arrives");
    Request {
        method,
        path,
        body,
        journal: None,
    }
}

fn open(
    cx: &mut TestAppContext,
    server: &Server,
    fixture: &Fixture,
    key: &SessionKey,
) -> (AnyWindowHandle, Entity<Workspace>) {
    // These tests deliberately cross real file/network worker threads.
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_global(Strings::load("en"));
        theme::apply(Palette::default_for(Appearance::Dark), cx);
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            window.set_layout_mode(gpui_kit::LayoutMode::Logical);
            cx.new(|cx| {
                let mut workspace =
                    Workspace::new(Source::Synthetic(WorkloadSize::Small), window, cx);
                let config = ServerConfig {
                    endpoint: server.endpoint.clone(),
                    password: None,
                    client: ClientDescription::default(),
                };
                let mut connection = Connection::new(config).expect("test network runtime starts");
                connection.client = Some(
                    ServerClient::new(server.endpoint.clone(), Credential::None)
                        .expect("anonymous loopback client builds"),
                );
                connection.state = ConnectionState::Connected;
                workspace.connection = Some(connection);
                workspace.switch_runtime(key.runtime.clone(), window, cx);
                workspace.open_persistence(fixture.journal(), window, cx);
                workspace
            })
        })
        .expect("headless window opens")
    })
}

fn wait(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "background work completes within five seconds"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

fn ready(cx: &mut TestAppContext, workspace: &Entity<Workspace>) {
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).persistence_ready())
    });
}

fn close(cx: &mut TestAppContext, window: AnyWindowHandle, workspace: &Entity<Workspace>) {
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| workspace.request_close(window, cx));
    })
    .expect("window is open before close");
    wait(cx, |cx| cx.update_window(window, |_, _, _| ()).is_err());
}

fn configure_send(
    workspace: &mut Workspace,
    key: &SessionKey,
    window: &mut gpui_kit::Window,
    cx: &mut gpui_kit::Context<Workspace>,
) {
    let model = ModelChoice {
        provider_id: "provider".to_owned(),
        model_id: "model".to_owned(),
        variant: Some("effort".to_owned()),
    };
    let mut session = SessionRecord::new(key.session.clone(), "/project");
    session.title = "Recovery test".to_owned();
    session.model = Some(model);
    session.agent = Some("agent".to_owned());
    let load = workspace.sessions.begin_load();
    let _ = workspace.sessions.complete_load(load, vec![session]);
    let mut catalog = DirectoryCatalog::default();
    catalog.agents = Load::Ready(vec![AgentEntry {
        name: "agent".to_owned(),
        display_name: None,
        mode: None,
        hidden: false,
        model: None,
        color: None,
    }]);
    catalog.providers = Load::Ready(vec![CatalogProvider {
        id: "provider".to_owned(),
        name: None,
        models: vec![CatalogModel {
            id: "model".to_owned(),
            provider_model_id: None,
            name: None,
            variants: vec!["effort".to_owned()],
            context_limit: None,
        }],
    }]);
    catalog.opencode = Load::Ready(Default::default());
    catalog.skills = Load::Ready(0);
    catalog.mcp_connected = Load::Ready(0);
    workspace.send_catalog.settings = Some(Default::default());
    workspace
        .send_catalog
        .insert_test_directory("/project".to_owned(), catalog);
    workspace.select_session(key.session.clone(), window, cx);
}

#[gpui_kit::test]
fn initial_send_is_durable_before_any_mutation_and_preserves_newer_typing(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("first-send-runtime", "ses_first_send");
    let server = Server::new(fixture.journal(), false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx);
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("first durable prompt", window, cx)
            });
            workspace.submit_connected(window, cx);
            assert_eq!(
                workspace.composer.read(cx).value().as_ref(),
                "first durable prompt",
                "the field keeps the text until its save barrier completes"
            );
        });
    })
    .expect("window open");
    wait(cx, |cx| {
        cx.read(|cx| {
            server.prompt_count() == 1 && !workspace.read(cx).send_attempts.is_active(&key)
        })
    });
    let id = cx.read(|cx| {
        workspace
            .read(cx)
            .send_attempts
            .attempt(&key)
            .expect("uncertain retained")
            .prompt
            .message_id
            .clone()
    });
    for request in server
        .requests
        .lock()
        .expect("requests")
        .iter()
        .filter(|request| request.method == "POST")
    {
        let snapshot = request
            .journal
            .as_ref()
            .expect("durable file precedes mutation");
        assert_eq!(snapshot["attempts"][0]["prompt"]["message_id"], id.as_str());
        assert_eq!(
            snapshot["attempts"][0]["prompt"]["text"],
            "first durable prompt"
        );
        assert!(
            snapshot["drafts"]
                .as_array()
                .expect("draft list")
                .is_empty(),
            "preparing text belongs to the retained attempt"
        );
    }
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("newer unsent typing", window, cx)
            });
        });
    })
    .expect("window open");
    close(cx, window, &workspace);
    let (_journal, loaded) =
        MutationJournal::open(&fixture.journal()).expect("closed lock released");
    let LoadedJournal::Present(snapshot) = loaded else {
        panic!("snapshot exists")
    };
    assert_eq!(snapshot.attempts[0].1.prompt.message_id, id);
    assert_eq!(snapshot.attempts[0].1.prompt.text, "first durable prompt");
    assert_eq!(snapshot.drafts[0].1.text, "newer unsent typing");
}

#[gpui_kit::test]
fn failed_close_keeps_second_writer_locked_and_retry_drains_memory(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    {
        let (mut journal, _) = MutationJournal::open(&fixture.journal()).expect("fixture opens");
        journal
            .save(&JournalSnapshot::default())
            .expect("empty snapshot saved");
    }
    let key = tests::key("close-retry-runtime", "close-retry-session");
    let server = Server::new(fixture.journal(), false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    let owner = tests::attempt(&key).owner;
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.composer_key = Some(owner.clone());
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("retained after failed close", window, cx)
            });
        });
    })
    .expect("window open");
    let backup = fixture.0.join("old-state.json");
    fs::rename(fixture.journal(), &backup).expect("preserve prior fixture state");
    fs::create_dir(fixture.journal()).expect("force a rename failure");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| workspace.request_close(window, cx))
    })
    .expect("window open");
    wait(cx, |cx| {
        cx.read(|cx| {
            matches!(
                workspace.read(cx).persistence_notice(),
                Some(crate::recovery_view::StorageNotice::Io)
            )
        })
    });
    fs::remove_dir(fixture.journal()).expect("remove empty test obstacle");
    fs::rename(backup, fixture.journal()).expect("restore valid stored state");
    assert!(
        matches!(
            MutationJournal::open(&fixture.journal()),
            Err(crate::mutation_journal::JournalError::Locked)
        ),
        "a failed close retains exclusive ownership until its retry is durable"
    );
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.retry_persistence(window, cx);
            workspace.request_close(window, cx);
        });
    })
    .expect("window remains open during recovery");
    wait(cx, |cx| cx.update_window(window, |_, _, _| ()).is_err());
    let (_journal, loaded) =
        MutationJournal::open(&fixture.journal()).expect("recovery close releases lock");
    let LoadedJournal::Present(snapshot) = loaded else {
        panic!("snapshot exists")
    };
    assert_eq!(
        snapshot
            .drafts
            .iter()
            .find(|(key, _)| key == &owner)
            .expect("retained draft")
            .1
            .text,
        "retained after failed close"
    );
}

#[gpui_kit::test]
fn exact_lookup_delivery_survives_failed_history_refresh(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("delivered-runtime", "ses_delivered");
    fixture.seed(&key);
    let attempt = tests::attempt(&key);
    let id = attempt.prompt.message_id.clone();
    let response = serde_json::json!({
        "data": {
            "id": id.as_str(),
            "type": "user",
            "time": { "created": 1792691938000_u64 },
            "text": "Prepared by the server"
        }
    });
    let server = Server::with_delivered(
        fixture.journal(),
        false,
        Some((
            format!("/api/session/{}/message/{id}", key.session),
            response.to_string(),
        )),
    );
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx);
            let open = workspace
                .open_sessions
                .get_mut(&key.session)
                .expect("visited session");
            let change = open.transcript.push(attempt.optimistic.clone());
            crate::server_sync::apply_list_change(open, &change, cx);
        });
    })
    .expect("window open");
    wait(cx, |cx| {
        cx.read(|cx| {
            matches!(
                workspace.read(cx).open_sessions[&key.session].history,
                crate::workspace::History::Failed(_)
            )
        })
    });
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.check_recovery(&key, window, cx)
        });
    })
    .expect("window open");
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            workspace.send_attempts.attempt(&key).is_none()
                && matches!(
                    workspace.open_sessions[&key.session].history,
                    crate::workspace::History::Failed(_)
                )
        })
    });
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        let transcript = &workspace.open_sessions[&key.session].transcript;
        let message = transcript
            .message(transcript.row_of(&id).expect("delivered row remains"))
            .expect("delivered message remains");
        assert_eq!(message.plain_text(), "Prepared by the server");
        assert_ne!(message, &attempt.optimistic);
        assert!(workspace.recovery_items(cx).is_empty());
    });
    assert_eq!(server.prompt_count(), 0, "lookup never resends");
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn explicit_retry_checks_identity_and_persists_before_post_then_survives_reopen(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("recovery-runtime", "recovery-session");
    fixture.seed(&key);
    let server = Server::new(fixture.journal(), false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.check_recovery(&key, window, cx)
        });
    })
    .expect("recovery window is open");
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace
                .read(cx)
                .recovery_items(cx)
                .iter()
                .any(|row| row.can_retry)
        })
    });
    assert_eq!(
        server.prompt_count(),
        0,
        "an absent lookup never retries automatically"
    );
    let before_retry = server.requests.lock().expect("request lock").len();
    // Force the retry barrier to recreate the snapshot. Reading the seeded file
    // alone would not prove that this dispatch waited for persistence.
    fs::remove_file(fixture.journal()).expect("only the fixture journal is removed");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.retry_recovery(&key, window, cx)
        });
    })
    .expect("recovery window is open");
    wait(cx, |cx| {
        cx.read(|cx| {
            server.prompt_count() == 1
                && workspace
                    .read(cx)
                    .recovery_items(cx)
                    .iter()
                    .any(|row| row.can_retry)
        })
    });
    {
        let requests = server.requests.lock().expect("request lock");
        let retry = &requests[before_retry..];
        assert_eq!(retry[0].method, "GET");
        assert!(retry[0].path.ends_with("/inbox"));
        assert_eq!(retry[1].method, "GET");
        assert!(
            retry[1]
                .path
                .contains(tests::attempt(&key).prompt.message_id.as_str())
        );
        let expected = tests::attempt(&key);
        for request in retry.iter().filter(|request| request.method == "POST") {
            let journal = request
                .journal
                .as_ref()
                .expect("journal exists before every POST");
            assert_eq!(
                journal["attempts"][0]["prompt"]["message_id"],
                expected.prompt.message_id.as_str()
            );
            assert_eq!(
                journal["attempts"][0]["prompt"]["text"],
                expected.prompt.text
            );
            assert_eq!(journal["attempts"][0]["runtime"], key.runtime.as_str());
            assert_eq!(journal["attempts"][0]["session"], key.session.as_str());
        }
        let prompt = retry
            .iter()
            .find(|request| request.path.ends_with("/prompt"))
            .expect("prompt was sent");
        let body: Value = serde_json::from_slice(&prompt.body).expect("prompt JSON");
        assert_eq!(body["id"], expected.prompt.message_id.as_str());
        assert_eq!(body["text"], expected.prompt.text);
    }
    close(cx, window, &workspace);
    let (window, reopened) = open(cx, &server, &fixture, &key);
    ready(cx, &reopened);
    cx.read(|cx| {
        let retained = reopened
            .read(cx)
            .send_attempts
            .attempt(&key)
            .expect("failed response retains the attempt");
        let expected = tests::attempt(&key);
        assert_eq!(retained.prompt.message_id, expected.prompt.message_id);
        assert_eq!(retained.draft.text, expected.draft.text);
        assert_eq!(retained.draft.selection, expected.draft.selection);
        assert_eq!(retained.admission, Admission::Uncertain);
    });
    assert_eq!(server.prompt_count(), 1, "reopening never resends");
    close(cx, window, &reopened);
}

#[gpui_kit::test]
fn corrupt_and_locked_journals_gate_mutations(cx: &mut TestAppContext) {
    for corrupt in [false, true] {
        let fixture = Fixture::new();
        let key = tests::key("blocked-runtime", "blocked-session");
        fixture.seed(&key);
        let lock = if corrupt {
            fs::write(fixture.journal(), b"{broken").expect("fixture journal corrupted");
            None
        } else {
            Some(
                MutationJournal::open(&fixture.journal())
                    .expect("fixture holds exclusive lock")
                    .0,
            )
        };
        let server = Server::new(fixture.journal(), false);
        let (window, workspace) = open(cx, &server, &fixture, &key);
        wait(cx, |cx| {
            cx.read(|cx| {
                matches!(
                    workspace.read(cx).persistence_notice(),
                    Some(
                        crate::recovery_view::StorageNotice::Locked
                            | crate::recovery_view::StorageNotice::Invalid
                    )
                )
            })
        });
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                assert!(!workspace.persistence_ready());
                let mut attempt = tests::attempt(&key);
                attempt.admission = Admission::Uncertain;
                workspace.send_attempts =
                    crate::send_attempt::SendAttempts::from_snapshot(vec![(key.clone(), attempt)])
                        .expect("retained attempt is valid even when storage is blocked");
                workspace.submit_connected(window, cx);
                workspace.retry_recovery(&key, window, cx);
                assert!(workspace.persistence_barrier(cx).is_none());
            });
        })
        .expect("blocked window is open");
        cx.run_until_parked();
        assert!(server.requests.lock().expect("request lock").is_empty());
        if corrupt {
            assert_eq!(
                fs::read(fixture.journal()).expect("corrupt journal remains"),
                b"{broken"
            );
        }
        close(cx, window, &workspace);
        drop(lock);
    }
}

#[gpui_kit::test]
fn normal_close_flushes_latest_dirty_draft_and_releases_lock(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("close-runtime", "close-session");
    let server = Server::new(fixture.journal(), false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    let owner = tests::attempt(&key).owner;
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.composer_key = Some(owner.clone());
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("older draft", window, cx)
            });
            workspace.queue_persistence(cx);
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("latest unsaved draft", window, cx)
            });
            workspace.queue_persistence(cx);
        });
    })
    .expect("draft window is open");
    close(cx, window, &workspace);
    let (_journal, loaded) =
        MutationJournal::open(&fixture.journal()).expect("normal close releases the lock");
    let LoadedJournal::Present(snapshot) = loaded else {
        panic!("close writes a snapshot")
    };
    assert_eq!(
        snapshot
            .drafts
            .iter()
            .find(|(key, _)| key == &owner)
            .expect("draft survives close")
            .1
            .text,
        "latest unsaved draft"
    );
    assert!(server.requests.lock().expect("request lock").is_empty());
}

#[gpui_kit::test]
fn late_retry_response_preserves_new_runtime_composer(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("old-runtime", "old-session");
    fixture.seed(&key);
    let server = Server::new(fixture.journal(), true);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.retry_recovery(&key, window, cx)
        });
    })
    .expect("old runtime window is open");
    wait(cx, |_| server.prompt_count() == 1);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.switch_runtime(RuntimeId::from("new-runtime"), window, cx);
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("new owner text", window, cx)
            });
        });
    })
    .expect("runtime switches while response is held");
    server.release_prompt.store(true, Ordering::Release);
    wait(cx, |cx| {
        cx.read(|cx| !workspace.read(cx).send_attempts.is_active(&key))
    });
    cx.read(|cx| {
        assert_eq!(
            workspace.read(cx).composer.read(cx).value().as_ref(),
            "new owner text"
        );
        let retained = workspace
            .read(cx)
            .send_attempts
            .attempt(&key)
            .expect("old runtime keeps uncertain attempt");
        assert_eq!(retained.admission, Admission::Uncertain);
        assert_eq!(
            retained.prompt.message_id,
            tests::attempt(&key).prompt.message_id
        );
    });
    assert_eq!(server.prompt_count(), 1);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn pending_send_check_is_reachable_by_keyboard(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("keyboard-runtime", "ses_keyboard");
    fixture.seed(&key);
    let server = Server::new(fixture.journal(), false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    let control = format!(
        "recovery-{}:{}:{}-check",
        key.runtime.as_str().len(),
        key.runtime,
        key.session
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let mut found = false;
        for _ in 0..120 {
            window.press("tab", cx);
            window.render_frame(cx);
            if window
                .find(gpui_kit::ElementId::Name(control.clone().into()))
                .focused()
                == Some(true)
            {
                window.press("space", cx);
                found = true;
                break;
            }
        }
        assert!(
            found,
            "pending-send status must be available through Tab and Space"
        );
    })
    .expect("window open");
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace
                .read(cx)
                .recovery_items(cx)
                .iter()
                .any(|row| row.can_retry)
        })
    });
    assert_eq!(
        server.prompt_count(),
        0,
        "checking status never submits a prompt"
    );
    close(cx, window, &workspace);
}
