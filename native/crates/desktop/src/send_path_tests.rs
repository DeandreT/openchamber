//! Headless send-path tests with real journal workers and a scripted server
//! that admits context and prompts: several attempts in one session, steering
//! while busy, abort exclusion and multi-step recovery.

use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::ids::{MessageId, SessionId};
use openchamber_native_core::session::LiveStatus;
use openchamber_native_core::session_index::SessionChange;
use openchamber_native_transport::event::ServerEvent;
use openchamber_native_transport::prompt::Delivery;
use openchamber_native_transport::prompt_parts::SyntheticContext;

use super::*;

/// Inbox items and history records the scripted server reports.
#[derive(Default)]
struct Script {
    inbox: Vec<(String, &'static str)>,
    history: Vec<(String, &'static str)>,
    hold_prompt: bool,
    /// POST operations (`model`, `prompt`, ...) answered with a 400.
    refuse: Vec<&'static str>,
    /// POST operations answered with a 500, an answer that proves nothing.
    fail: Vec<&'static str>,
    /// Sessions `GET /api/session/:id` finds, at this directory.
    sessions: Vec<(String, &'static str)>,
}

struct Scripted {
    endpoint: Endpoint,
    requests: Arc<Mutex<Vec<Request>>>,
    script: Arc<Mutex<Script>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Scripted {
    fn new(journal: PathBuf, script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener binds");
        listener.set_nonblocking(true).expect("nonblocking");
        let endpoint = Endpoint::parse(&format!(
            "http://{}",
            listener.local_addr().expect("address")
        ))
        .expect("endpoint");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(script));
        let stop = Arc::new(AtomicBool::new(false));
        let (recorded, scripted, stopped) = (requests.clone(), script.clone(), stop.clone());
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
                if request.method == "POST" {
                    request.journal = fs::read(&journal)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
                }
                let (status, body) = answer(&request, &scripted.lock().expect("script"));
                let held = request.path.ends_with("/prompt");
                recorded.lock().expect("requests").push(request);
                let deadline = Instant::now() + Duration::from_secs(5);
                while held
                    && scripted.lock().expect("script").hold_prompt
                    && !stopped.load(Ordering::Acquire)
                {
                    assert!(Instant::now() < deadline, "test releases the held prompt");
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
            script,
            stop,
            worker: Some(worker),
        }
    }

    fn server(&self) -> Server {
        // `open` reads only the endpoint; this server keeps its own worker.
        Server {
            endpoint: self.endpoint.clone(),
            requests: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(true)),
            release_prompt: Arc::new(AtomicBool::new(true)),
            worker: None,
        }
    }

    /// `operation:id` for each admission and selection POST, in order.
    fn posts(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("requests")
            .iter()
            .filter(|request| request.method == "POST")
            .map(|request| {
                let operation = request.path.rsplit('/').next().unwrap_or_default();
                let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
                match body.get("id").and_then(Value::as_str) {
                    Some(id) => format!("{operation}:{id}"),
                    None => operation.to_owned(),
                }
            })
            .filter(|operation| !operation.starts_with("model") && !operation.starts_with("agent"))
            .collect()
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

fn answer(request: &Request, script: &Script) -> (&'static str, String) {
    let session = request
        .path
        .strip_prefix("/api/session/")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default()
        .to_owned();
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let receipt = |kind: &str| {
        serde_json::json!({"data": {
            "id": body["id"], "sessionID": session, "type": kind,
            "time": {"created": 1}, "delivery": body["delivery"].as_str().unwrap_or("queue"),
            "payload": {}
        }})
        .to_string()
    };
    let path = request.path.as_str();
    let operation = path.rsplit('/').next().unwrap_or_default();
    if request.method == "POST" && script.refuse.contains(&operation) {
        return ("400 Bad Request", "{}".to_owned());
    }
    if request.method == "POST" && script.fail.contains(&operation) {
        return ("500 Internal Server Error", "{}".to_owned());
    }
    if request.method == "GET"
        && let Some((id, directory)) = script
            .sessions
            .iter()
            .find(|(id, _)| path == format!("/api/session/{id}"))
    {
        return (
            "200 OK",
            serde_json::json!({"data": {
                "id": id, "projectID": "global", "location": {"directory": directory},
                "title": "Created", "cost": 0,
                "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                "time": {"created": 1, "updated": 1}
            }})
            .to_string(),
        );
    }
    match request.method.as_str() {
        "POST" if path.ends_with("/synthetic") => ("200 OK", receipt("synthetic")),
        "POST" if path.ends_with("/prompt") => ("200 OK", receipt("user")),
        "POST" if path.ends_with("/interrupt") => ("200 OK", r#"{"interrupted":true}"#.to_owned()),
        "POST" => ("204 No Content", String::new()),
        _ if path.ends_with("/inbox") => {
            let items: Vec<Value> = script
                .inbox
                .iter()
                .map(|(id, kind)| {
                    serde_json::json!({"id": id, "sessionID": session, "type": kind,
                        "time": {"created": 1}, "delivery": "steer", "payload": {"text": "x"}})
                })
                .collect();
            ("200 OK", serde_json::json!({ "data": items }).to_string())
        }
        _ => {
            let id = path.rsplit('/').next().unwrap_or_default();
            match script.history.iter().find(|(known, _)| known == id) {
                Some((id, kind)) => (
                    "200 OK",
                    serde_json::json!({"data": {"id": id, "type": kind, "text": "x",
                        "time": {"created": 1}}})
                    .to_string(),
                ),
                None => ("404 Not Found", "{}".to_owned()),
            }
        }
    }
}

fn type_and(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    text: &str,
    delivery: Option<Delivery>,
) {
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace
                .composer
                .update(cx, |composer, cx| composer.set_value(text, window, cx));
            workspace.submit_connected_with(delivery, window, cx);
        })
    })
    .expect("window");
}

fn idle(cx: &mut TestAppContext, workspace: &Entity<Workspace>, key: &SessionKey) {
    wait(cx, |cx| {
        cx.read(|cx| !workspace.read(cx).send_attempts.is_active(key))
    });
}

#[gpui_kit::test]
fn steer_while_busy_runs_beside_an_acknowledged_send_and_excludes_abort(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("steer-runtime", "ses_steer");
    let scripted = Scripted::new(fixture.journal(), Script::default());
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx)
        })
    })
    .expect("window");

    type_and(cx, window, &workspace, "first", None);
    idle(cx, &workspace, &key);
    let first = cx.read(|cx| workspace.read(cx).send_attempts.ids(&key));
    assert_eq!(first.len(), 1, "admission keeps the attempt until its echo");
    cx.update(|cx| {
        workspace.update(cx, |workspace, _| {
            workspace
                .live_status
                .insert(key.session.clone(), LiveStatus::Busy)
        })
    });

    type_and(cx, window, &workspace, "plain while busy", None);
    cx.run_until_parked();
    assert_eq!(
        scripted.posts().len(),
        1,
        "a plain send while busy waits for the queue path"
    );
    assert_eq!(
        cx.read(|cx| workspace.read(cx).composer.read(cx).value().to_string()),
        "plain while busy",
        "the refused text stays in the composer"
    );

    scripted.script.lock().expect("script").hold_prompt = true;
    type_and(cx, window, &workspace, "steer now", Some(Delivery::Steer));
    wait(cx, |_| scripted.posts().len() == 2);
    let steer = cx.read(|cx| {
        let ids = workspace.read(cx).send_attempts.ids(&key);
        assert_eq!(ids.len(), 2, "two unresolved attempts in one session");
        ids[1].clone()
    });
    {
        let requests = scripted.requests.lock().expect("requests");
        let prompt = requests
            .iter()
            .rfind(|request| request.path.ends_with("/prompt"))
            .expect("steer prompt");
        let body: Value = serde_json::from_slice(&prompt.body).expect("JSON");
        assert_eq!(body["delivery"], "steer");
        assert_eq!(body["text"], "steer now");
        let journal = prompt.journal.as_ref().expect("journal before contact");
        assert_eq!(
            journal["attempts"].as_array().expect("attempts").len(),
            2,
            "the steer is durable beside the acknowledged send before its POST"
        );
    }
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| workspace.abort_connected(window, cx))
    })
    .expect("window");
    assert!(
        !scripted.posts().iter().any(|post| post == "interrupt"),
        "abort waits while the steer chain runs"
    );
    scripted.script.lock().expect("script").hold_prompt = false;
    idle(cx, &workspace, &key);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.abort_connected(window, cx);
            assert!(workspace.send_attempts.is_active(&key));
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("during abort", window, cx)
            });
            workspace.submit_connected_with(Some(Delivery::Steer), window, cx);
        })
    })
    .expect("window");
    idle(cx, &workspace, &key);
    let posts = scripted.posts();
    assert_eq!(
        posts
            .iter()
            .filter(|post| post.starts_with("prompt"))
            .count(),
        2,
        "a steer refused during abort sends nothing: {posts:?}"
    );
    assert_eq!(posts.last().map(String::as_str), Some("interrupt"));

    // The first prompt's echo resolves only that attempt.
    scripted
        .script
        .lock()
        .expect("script")
        .history
        .push((first[0].to_string(), "user"));
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.check_recovery(&key, &first[0], window, cx)
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).send_attempts.ids(&key) == [steer.clone()])
    });
    close(cx, window, &workspace);
}

fn seed_steps(fixture: &Fixture, key: &SessionKey) -> (MessageId, [MessageId; 2]) {
    seed_steps_from(fixture, key, crate::send_attempt::SendOrigin::Existing, "")
}

/// An uncertain two-context attempt, plus a newer draft for its owner.
fn seed_steps_from(
    fixture: &Fixture,
    key: &SessionKey,
    origin: crate::send_attempt::SendOrigin,
    newer: &str,
) -> (MessageId, [MessageId; 2]) {
    let mut attempt = tests::attempt(key);
    attempt.origin = origin;
    attempt.admission = Admission::Uncertain;
    let context = [
        MessageId::from("msg_bcfe56800000Context0001"),
        MessageId::from("msg_bcfe56800000Context0002"),
    ];
    attempt.prompt.parts.context = context
        .iter()
        .map(|id| SyntheticContext {
            id: id.clone(),
            text: format!("context {id}"),
            description: None,
            metadata: None,
        })
        .collect();
    attempt.prompt.delivery = Some(Delivery::Steer);
    let drafts = if newer.is_empty() {
        Vec::new()
    } else {
        vec![(
            attempt.owner.clone(),
            openchamber_native_core::draft::Draft::new(newer),
        )]
    };
    let (mut journal, _) = MutationJournal::open(&fixture.journal()).expect("journal opens");
    journal
        .save(&JournalSnapshot {
            drafts,
            attempts: vec![(key.clone(), attempt)],
            creations: Vec::new(),
            selections: Vec::new(),
        })
        .expect("attempt saves");
    (tests::sent_id(), context)
}

#[gpui_kit::test]
fn multi_step_recovery_resends_only_missing_steps_in_order(cx: &mut TestAppContext) {
    for observed in [1, 2] {
        let fixture = Fixture::new();
        let key = tests::key("steps-runtime", "ses_steps");
        let (prompt, context) = seed_steps(&fixture, &key);
        let scripted = Scripted::new(
            fixture.journal(),
            Script {
                inbox: context[..observed]
                    .iter()
                    .map(|id| (id.to_string(), "synthetic"))
                    .collect(),
                ..Script::default()
            },
        );
        let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
        ready(cx, &workspace);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                configure_send(workspace, &key, window, cx);
                workspace.retry_recovery(&key, &prompt, window, cx);
            })
        })
        .expect("window");
        wait(cx, |_| {
            scripted
                .posts()
                .iter()
                .any(|post| post.starts_with("prompt"))
        });
        idle(cx, &workspace, &key);
        let mut expected: Vec<String> = context[observed..]
            .iter()
            .map(|id| format!("synthetic:{id}"))
            .collect();
        expected.push(format!("prompt:{prompt}"));
        assert_eq!(
            scripted.posts(),
            expected,
            "with {observed} context steps observed, only the rest are resent"
        );
        for request in scripted.requests.lock().expect("requests").iter() {
            if request.path.ends_with("/synthetic") || request.path.ends_with("/prompt") {
                let body: Value = serde_json::from_slice(&request.body).expect("JSON");
                assert_eq!(body["delivery"], "steer", "every step keeps its delivery");
            }
        }
        assert_eq!(
            cx.read(|cx| {
                workspace
                    .read(cx)
                    .send_attempts
                    .attempt(&key, &prompt)
                    .map(|attempt| attempt.admission)
            }),
            Some(Admission::Admitted)
        );
        close(cx, window, &workspace);
    }
}

#[gpui_kit::test]
fn out_of_order_context_keeps_the_attempt_uncertain_without_a_resend(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("gap-runtime", "ses_gap");
    let (prompt, context) = seed_steps(&fixture, &key);
    let scripted = Scripted::new(
        fixture.journal(),
        Script {
            history: vec![(context[1].to_string(), "synthetic")],
            ..Script::default()
        },
    );
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx);
            workspace.retry_recovery(&key, &prompt, window, cx);
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace.read(cx).recovery_items(cx).iter().any(|row| {
                row.message == prompt
                    && row.phase == crate::recovery_view::RecoveryPhase::OutOfOrder
            })
        })
    });
    assert!(
        scripted.posts().is_empty(),
        "no step is resent out of order"
    );
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        let row = workspace
            .recovery_items(cx)
            .into_iter()
            .find(|row| row.message == prompt)
            .expect("row");
        assert!(!row.can_retry);
        assert!(row.can_restore, "the only way out is the composer");
        assert_eq!(
            workspace
                .send_attempts
                .attempt(&key, &prompt)
                .expect("kept")
                .admission,
            Admission::Uncertain
        );
    });
    restore_and_restart(
        cx,
        (window, workspace),
        &scripted,
        &fixture,
        (&key, &prompt),
        "submitted text",
    );
}

/// Moves the attempt to the composer through its row, then checks the
/// journal on disk and a restarted window: the attempt is gone for good and
/// its text is in the session's draft.
fn restore_and_restart(
    cx: &mut TestAppContext,
    (window, workspace): (AnyWindowHandle, Entity<Workspace>),
    scripted: &Scripted,
    fixture: &Fixture,
    (key, prompt): (&SessionKey, &MessageId),
    expected: &str,
) {
    let sent = scripted.posts().len();
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.restore_recovery(key, prompt, window, cx)
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            workspace.send_attempts.attempt(key, prompt).is_none()
                && workspace.recovery_items(cx).is_empty()
        })
    });
    wait(cx, |_| {
        fs::read(fixture.journal())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|stored| stored["attempts"] == serde_json::json!([]))
    });
    let stored: Value =
        serde_json::from_slice(&fs::read(fixture.journal()).expect("journal")).expect("JSON");
    assert_eq!(
        stored["drafts"][0]["text"], expected,
        "the text went back to its draft in the same snapshot"
    );
    assert_eq!(
        scripted.posts().len(),
        sent,
        "moving to the composer sends nothing"
    );
    close(cx, window, &workspace);
    let (window, reopened) = open(cx, &scripted.server(), fixture, key);
    ready(cx, &reopened);
    cx.update_window(window, |_, window, cx| {
        reopened.update(cx, |workspace, cx| {
            configure_send(workspace, key, window, cx);
            assert!(workspace.send_attempts.session(key).next().is_none());
            assert_eq!(workspace.composer.read(cx).value().as_ref(), expected);
            assert!(
                workspace.send_attempts.can_send(key),
                "the session is unblocked"
            );
        })
    })
    .expect("window");
    close(cx, window, &reopened);
}

#[gpui_kit::test]
fn a_refused_retry_offers_the_composer_after_newer_text(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("refused-runtime", "ses_refused");
    let (prompt, _) = seed_steps_from(
        &fixture,
        &key,
        crate::send_attempt::SendOrigin::Existing,
        "newer text",
    );
    // The saved model was removed: every retry's selection is refused.
    let scripted = Scripted::new(
        fixture.journal(),
        Script {
            refuse: vec!["model"],
            ..Script::default()
        },
    );
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    let restorable = |workspace: &Workspace, cx: &gpui_kit::App| {
        workspace
            .recovery_items(cx)
            .iter()
            .any(|row| row.can_restore)
    };
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx);
            assert!(
                !restorable(workspace, cx),
                "an unchecked uncertain attempt may still resolve"
            );
            workspace.retry_recovery(&key, &prompt, window, cx);
            assert!(
                !restorable(workspace, cx),
                "never while a check or retry runs"
            );
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace.read(cx).recovery_items(cx).iter().any(|row| {
                row.message == prompt
                    && row.phase == crate::recovery_view::RecoveryPhase::Refused
                    && row.can_restore
                    && !row.can_retry
            })
        })
    });
    assert!(
        !scripted
            .posts()
            .iter()
            .any(|post| post.starts_with("prompt")),
        "the refused selection stopped the retry before its prompt"
    );
    restore_and_restart(
        cx,
        (window, workspace),
        &scripted,
        &fixture,
        (&key, &prompt),
        "newer text\n\nsubmitted text",
    );
}

#[gpui_kit::test]
fn a_created_session_retry_skips_its_admitted_context(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("created-runtime", "ses_createdsteps");
    let (prompt, context) =
        seed_steps_from(&fixture, &key, crate::send_attempt::SendOrigin::Created, "");
    let scripted = Scripted::new(
        fixture.journal(),
        Script {
            inbox: vec![(context[0].to_string(), "synthetic")],
            sessions: vec![(key.session.to_string(), "/project")],
            ..Script::default()
        },
    );
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx);
            workspace.retry_recovery(&key, &prompt, window, cx);
        })
    })
    .expect("window");
    wait(cx, |_| {
        scripted
            .posts()
            .iter()
            .any(|post| post.starts_with("prompt"))
    });
    idle(cx, &workspace, &key);
    assert_eq!(
        scripted.posts(),
        [
            format!("synthetic:{}", context[1]),
            format!("prompt:{prompt}")
        ],
        "the observed context is not admitted twice"
    );
    close(cx, window, &workspace);
}

/// Saves `attempts`, `creations` and `drafts` as the journal a window opens.
fn seed_journal(
    fixture: &Fixture,
    attempts: Vec<(SessionKey, crate::send_attempt::SendAttempt)>,
    creations: Vec<(SessionKey, crate::create_attempt::CreateAttempt)>,
    drafts: Vec<(DraftKey, openchamber_native_core::draft::Draft)>,
) {
    let (mut journal, _) = MutationJournal::open(&fixture.journal()).expect("journal opens");
    journal
        .save(&JournalSnapshot {
            drafts,
            attempts,
            creations,
            selections: Vec::new(),
        })
        .expect("journal saves");
}

/// `attempt` under its own message and part IDs, so several fit one journal.
fn distinct(
    mut attempt: crate::send_attempt::SendAttempt,
    suffix: &str,
    admission: Admission,
) -> crate::send_attempt::SendAttempt {
    attempt.admission = admission;
    attempt.prompt.message_id = MessageId::from(format!("msg_bcfe5680000{suffix}AbCdEf01234567"));
    attempt.optimistic.info.id = attempt.prompt.message_id.clone();
    attempt.optimistic.parts[0].id =
        openchamber_native_core::ids::PartId::from(format!("local-part-{suffix}"));
    attempt
}

fn session_draft(key: &SessionKey) -> DraftKey {
    DraftKey::new(
        key.runtime.clone(),
        "/project",
        DraftTarget::Session(key.session.clone()),
    )
    .expect("test directory is absolute")
}

fn delete_session(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    session: &SessionId,
) {
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.apply_server_events(
                vec![ServerEvent::SessionDeleted {
                    session_id: session.clone(),
                }],
                window,
                cx,
            )
        })
    })
    .expect("window");
}

/// Waits until nothing in memory or in the journal on disk names `deleted`,
/// while `kept` still has its draft.
fn assert_forgotten(
    cx: &mut TestAppContext,
    workspace: &Entity<Workspace>,
    fixture: &Fixture,
    deleted: &[&SessionId],
    kept: Option<&str>,
) {
    let names = |value: &Value| {
        let text = value.to_string();
        deleted
            .iter()
            .any(|session| text.contains(session.as_str()))
    };
    wait(cx, |cx| {
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            !workspace
                .recovery_items(cx)
                .iter()
                .any(|row| deleted.contains(&&row.key.session))
                && !workspace
                    .send_attempts
                    .iter()
                    .any(|(key, _)| deleted.contains(&&key.session))
        })
    });
    wait(cx, |_| {
        fs::read(fixture.journal())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|stored| {
                !names(&stored["attempts"])
                    && !names(&stored["creations"])
                    && !names(&stored["drafts"])
            })
    });
    if let Some(kept) = kept {
        let stored: Value =
            serde_json::from_slice(&fs::read(fixture.journal()).expect("journal")).expect("JSON");
        assert!(
            stored["drafts"].to_string().contains(kept),
            "an unrelated session's draft stays"
        );
    }
}

#[gpui_kit::test]
fn deleting_a_session_drops_its_admitted_and_uncertain_attempts_and_drafts(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("deleted-runtime", "ses_deletedsends");
    let child = SessionKey {
        session: SessionId::from("ses_deletedchild"),
        ..key.clone()
    };
    let other = SessionKey {
        session: SessionId::from("ses_survivor"),
        ..key.clone()
    };
    let mut admitted = tests::attempt(&key);
    admitted.admission = Admission::Admitted;
    let uncertain = distinct(tests::attempt(&key), "2", Admission::Uncertain);
    let descendant = distinct(tests::attempt(&child), "3", Admission::Uncertain);
    seed_journal(
        &fixture,
        vec![
            (key.clone(), admitted),
            (key.clone(), uncertain),
            (child.clone(), descendant),
        ],
        Vec::new(),
        vec![
            (session_draft(&key), Draft::new("deleted draft")),
            (session_draft(&child), Draft::new("child draft")),
            (session_draft(&other), Draft::new("survivor draft")),
        ],
    );
    let scripted = Scripted::new(fixture.journal(), Script::default());
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx);
            let mut record = SessionRecord::new(child.session.clone(), "/project");
            record.parent_id = Some(key.session.clone());
            let _ = workspace.sessions.apply([SessionChange::Created(record)]);
            assert_eq!(
                workspace.recovery_items(cx).len(),
                3,
                "both attempts and the child's are restored"
            );
        })
    })
    .expect("window");
    delete_session(cx, window, &workspace, &key.session);
    assert_forgotten(
        cx,
        &workspace,
        &fixture,
        &[&key.session, &child.session],
        Some("survivor draft"),
    );
    close(cx, window, &workspace);
    let (window, reopened) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &reopened);
    assert!(
        cx.read(|cx| reopened.read(cx).recovery_items(cx).is_empty()),
        "nothing comes back after a restart"
    );
    close(cx, window, &reopened);
}

#[gpui_kit::test]
fn deleting_a_created_session_drops_its_first_send_and_creation(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    // A creation still being confirmed, and one bound to its session whose
    // first send is unresolved.
    let (creating, mut creation) = crate::create_attempt::tests::sample();
    creation.phase = crate::create_attempt::CreationPhase::Confirmed;
    creation.navigation = None;
    let key = SessionKey {
        session: SessionId::from("ses_createdfirstsend"),
        ..creating.clone()
    };
    let mut first = distinct(tests::attempt(&key), "4", Admission::Uncertain);
    first.origin = crate::send_attempt::SendOrigin::Created;
    seed_journal(
        &fixture,
        vec![(key.clone(), first)],
        vec![(creating.clone(), creation)],
        Vec::new(),
    );
    let scripted = Scripted::new(fixture.journal(), Script::default());
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    assert_eq!(
        cx.read(|cx| workspace.read(cx).recovery_items(cx).len()),
        2,
        "the creation and the created session's first send are restored"
    );
    delete_session(cx, window, &workspace, &key.session);
    delete_session(cx, window, &workspace, &creating.session);
    assert_forgotten(
        cx,
        &workspace,
        &fixture,
        &[&key.session, &creating.session],
        None,
    );
    assert!(
        cx.read(|cx| workspace.read(cx).creations.get(&creating).is_none()),
        "no CreateDeleted entry is left behind"
    );
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn deleting_the_selected_session_does_not_save_its_composer_text(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("selected-runtime", "ses_selecteddelete");
    let scripted = Scripted::new(fixture.journal(), Script::default());
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx);
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("typed then deleted", window, cx)
            });
            workspace.save_composer_draft(cx);
            workspace.start_background_save(cx);
        })
    })
    .expect("window");
    wait(cx, |_| {
        fs::read_to_string(fixture.journal()).is_ok_and(|text| text.contains("typed then deleted"))
    });
    delete_session(cx, window, &workspace, &key.session);
    assert_forgotten(cx, &workspace, &fixture, &[&key.session], None);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn a_send_finishing_after_its_session_was_deleted_changes_nothing(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("inflight-runtime", "ses_inflightdelete");
    let scripted = Scripted::new(
        fixture.journal(),
        Script {
            hold_prompt: true,
            ..Script::default()
        },
    );
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx)
        })
    })
    .expect("window");
    type_and(cx, window, &workspace, "sent before delete", None);
    wait(cx, |_| {
        scripted
            .posts()
            .iter()
            .any(|post| post.starts_with("prompt"))
    });
    delete_session(cx, window, &workspace, &key.session);
    assert_forgotten(cx, &workspace, &fixture, &[&key.session], None);
    scripted.script.lock().expect("script").hold_prompt = false;
    idle(cx, &workspace, &key);
    assert_forgotten(cx, &workspace, &fixture, &[&key.session], None);
    let requests = scripted.posts().len();
    cx.run_until_parked();
    assert_eq!(
        scripted.posts().len(),
        requests,
        "the late result starts no check or resend"
    );
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn stop_answered_before_dispatch_keeps_the_prompt_unsent(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("stop-runtime", "ses_stopfirst");
    let scripted = Scripted::new(fixture.journal(), Script::default());
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx);
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("stopped text", window, cx)
            });
            workspace.submit_connected_with(None, window, cx);
            workspace.abort_connected(window, cx);
            // The interrupt answers while the send is still saving.
            workspace.finish_abort(&key, Some(Ok(true)), window, cx);
        })
    })
    .expect("window");
    idle(cx, &workspace, &key);
    cx.run_until_parked();
    assert!(
        !scripted
            .posts()
            .iter()
            .any(|post| post.starts_with("prompt")),
        "nothing is sent after Stop: {:?}",
        scripted.posts()
    );
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert_eq!(workspace.composer.read(cx).value().as_ref(), "stopped text");
        assert!(workspace.send_attempts.session(&key).next().is_none());
    });
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn a_lost_selection_answer_on_retry_checks_again_instead_of_refusing(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("lost-runtime", "ses_lostselection");
    let (prompt, _) = seed_steps(&fixture, &key);
    let scripted = Scripted::new(
        fixture.journal(),
        Script {
            fail: vec!["model"],
            ..Script::default()
        },
    );
    let (window, workspace) = open(cx, &scripted.server(), &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            configure_send(workspace, &key, window, cx);
            workspace.retry_recovery(&key, &prompt, window, cx);
        })
    })
    .expect("window");
    let model_posts = |scripted: &Scripted| {
        scripted
            .requests
            .lock()
            .expect("requests")
            .iter()
            .filter(|request| request.method == "POST" && request.path.ends_with("/model"))
            .count()
    };
    wait(cx, |_| model_posts(&scripted) == 1);
    idle(cx, &workspace, &key);
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace.read(cx).recovery_items(cx).iter().any(|row| {
                row.message == prompt
                    && row.phase == crate::recovery_view::RecoveryPhase::NotObserved
                    && row.can_retry
                    && !row.can_restore
            })
        })
    });
    assert!(
        !scripted
            .posts()
            .iter()
            .any(|post| post.starts_with("prompt")),
        "the lost selection stopped the retry before its prompt"
    );
    close(cx, window, &workspace);
}
