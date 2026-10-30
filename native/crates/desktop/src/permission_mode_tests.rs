//! The composer's permission shield against a scripted policy server: the
//! cycle, the safety-net gate, ancestry, failure retention, revision order,
//! runtime switches and which requests stay out of sight.

use gpui_kit::ElementId;
use openchamber_native_core::ids::{PermissionId, SessionId};
use openchamber_native_core::pending_requests::{PendingRequest, RequestId, RequestKey};
use openchamber_native_core::permission::PermissionRequest;
use openchamber_native_core::permission_mode::{PermissionMode, PolicySnapshot};
use openchamber_native_core::request_owner::{RequestOwner, RequestScope};
use openchamber_native_transport::event::{BlockingRequestEvent, ServerEvent};
use openchamber_native_transport::stream::StreamItem;
use serde_json::json;

use super::*;

#[derive(Default)]
struct Policy {
    modes: serde_json::Map<String, Value>,
    revision: u64,
    safety_net: bool,
    fail_reads: bool,
    fail_writes: bool,
    hold_writes: bool,
}

fn policy_server(policy: Arc<Mutex<Policy>>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = Endpoint::parse(&format!(
        "http://{}",
        listener.local_addr().expect("address")
    ))
    .expect("endpoint");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let release_prompt = Arc::new(AtomicBool::new(true));
    let recorded = requests.clone();
    let stopped = stop.clone();
    let worker = thread::spawn(move || {
        while !stopped.load(Ordering::Acquire) {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("timeout");
            // A request the app dropped before sending, such as a superseded
            // read, closes its connection without a byte; it is no request.
            if matches!(stream.peek(&mut [0]), Ok(0)) {
                continue;
            }
            let request = read_request(&mut stream);
            let snapshot = |policy: &Policy| {
                let sessions: serde_json::Map<String, Value> = policy
                    .modes
                    .iter()
                    .map(|(id, mode)| (id.clone(), json!(mode != "ask")))
                    .collect();
                json!({"sessions": sessions, "modes": policy.modes, "revision": policy.revision})
            };
            let write = request.method == "PUT"
                && request
                    .path
                    .starts_with("/api/permission-auto-accept/sessions/");
            // Recorded before a held write blocks, so tests can see it arrive.
            let (path, method, sent) = (
                request.path.clone(),
                request.method.clone(),
                request.body.clone(),
            );
            recorded.lock().expect("requests").push(request);
            let (status, body) = if path == "/api/routing" {
                let safety = policy.lock().expect("policy").safety_net;
                (
                    200,
                    json!({"available": true, "autoReady": false, "jevAvailable": safety}),
                )
            } else if method == "GET" && path == "/api/permission-auto-accept" {
                let policy = policy.lock().expect("policy");
                if policy.fail_reads {
                    (503, json!({}))
                } else {
                    (200, snapshot(&policy))
                }
            } else if write {
                while policy.lock().expect("policy").hold_writes && !stopped.load(Ordering::Acquire)
                {
                    thread::sleep(Duration::from_millis(1));
                }
                let mut policy = policy.lock().expect("policy");
                if policy.fail_writes {
                    (500, json!({"error": "fixture failure"}))
                } else {
                    let body: Value = serde_json::from_slice(&sent).expect("body");
                    let session = path.rsplit('/').next().expect("session segment").to_owned();
                    policy.modes.insert(session, body["mode"].clone());
                    policy.revision += 1;
                    (200, snapshot(&policy))
                }
            } else {
                (404, json!({}))
            };
            let body = body.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    Server {
        endpoint,
        requests,
        stop,
        release_prompt,
        worker: Some(worker),
    }
}

fn count(server: &Server, method: &str, prefix: &str) -> usize {
    server
        .requests
        .lock()
        .expect("requests")
        .iter()
        .filter(|request| request.method == method && request.path.starts_with(prefix))
        .count()
}

fn writes(server: &Server) -> Vec<Value> {
    server
        .requests
        .lock()
        .expect("requests")
        .iter()
        .filter(|request| request.method == "PUT")
        .map(|request| serde_json::from_slice(&request.body).expect("write body"))
        .collect()
}

/// Opens a connected window on `key`'s session and waits for the first
/// routing and policy reads.
fn open_policy(
    cx: &mut TestAppContext,
    server: &Server,
    fixture: &Fixture,
    key: &SessionKey,
) -> (AnyWindowHandle, Entity<Workspace>) {
    let (window, workspace) = open(cx, server, fixture, key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| configure_send(this, key, window, cx))
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| {
            let this = workspace.read(cx);
            this.send_catalog.routing_read.is_none() && this.permission_modes.policy.is_loaded()
        })
    });
    (window, workspace)
}

fn shown(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> PermissionMode {
    cx.read(|cx| workspace.read(cx).shown_permission_mode())
}

fn click_shield(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx| {
        window.click(ElementId::Name("composer-permissions".into()), cx)
    })
    .expect("window");
}

fn settle_writes(cx: &mut TestAppContext, workspace: &Entity<Workspace>) {
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).permission_modes.writes_in_flight() == 0)
    });
}

fn label(cx: &mut TestAppContext, window: AnyWindowHandle) -> Option<String> {
    cx.update_window(window, |_, window, _| {
        window
            .find(ElementId::Name("composer-permissions".into()))
            .label()
            .map(|label| label.to_string())
    })
    .expect("window")
}

#[gpui_kit::test]
fn the_shield_cycles_through_server_snapshots_and_gates_safety(cx: &mut TestAppContext) {
    for safety_net in [false, true] {
        let fixture = Fixture::new();
        let key = tests::key("policy-runtime", "ses_policy");
        let policy = Arc::new(Mutex::new(Policy {
            safety_net,
            ..Policy::default()
        }));
        let server = policy_server(policy.clone());
        let (window, workspace) = open_policy(cx, &server, &fixture, &key);
        assert_eq!(shown(cx, &workspace), PermissionMode::Ask);
        assert_eq!(
            label(cx, window).as_deref(),
            Some("Permissions: ask every time")
        );
        let expected = if safety_net {
            vec![
                PermissionMode::Safety,
                PermissionMode::Auto,
                PermissionMode::Ask,
            ]
        } else {
            vec![PermissionMode::Auto, PermissionMode::Ask]
        };
        for mode in &expected {
            click_shield(cx, window);
            settle_writes(cx, &workspace);
            assert_eq!(shown(cx, &workspace), *mode, "safety net {safety_net}");
        }
        let modes: Vec<Value> = writes(&server)
            .into_iter()
            .map(|body| {
                assert_eq!(body["enabled"], json!(body["mode"] != "ask"));
                assert_eq!(body["directory"], "/project");
                body["mode"].clone()
            })
            .collect();
        let names = |mode: &PermissionMode| match mode {
            PermissionMode::Ask => json!("ask"),
            PermissionMode::Safety => json!("safety"),
            PermissionMode::Auto => json!("auto"),
        };
        assert_eq!(modes, expected.iter().map(names).collect::<Vec<_>>());
        if !safety_net {
            // A stored Safety reads as Ask and cycles on to Auto.
            policy
                .lock()
                .expect("policy")
                .modes
                .insert("ses_policy".into(), json!("safety"));
            cx.update_window(window, |_, _, cx| {
                workspace.update(cx, |this, cx| this.load_permission_policy(cx))
            })
            .expect("window");
            wait(cx, |cx| {
                cx.read(|cx| {
                    workspace.read(cx).session_permission_mode(&key.session)
                        == PermissionMode::Safety
                })
            });
            assert_eq!(shown(cx, &workspace), PermissionMode::Ask);
            click_shield(cx, window);
            settle_writes(cx, &workspace);
            assert_eq!(shown(cx, &workspace), PermissionMode::Auto);
        }
        close(cx, window, &workspace);
    }
}

#[gpui_kit::test]
fn sessions_inherit_the_nearest_explicit_ancestor_and_drafts_their_own_choice(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_child");
    let policy = Arc::new(Mutex::new(Policy::default()));
    policy
        .lock()
        .expect("policy")
        .modes
        .insert("ses_root".into(), json!("auto"));
    let server = policy_server(policy);
    let (window, workspace) = open_policy(cx, &server, &fixture, &key);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            let mut child = this.sessions.get(&key.session).expect("child").clone();
            child.parent_id = Some("ses_middle".into());
            let mut middle = SessionRecord::new(SessionId::from("ses_middle"), "/project");
            middle.parent_id = Some("ses_root".into());
            let root = SessionRecord::new(SessionId::from("ses_root"), "/project");
            let mut looped = SessionRecord::new(SessionId::from("ses_loop"), "/project");
            looped.parent_id = Some("ses_loop".into());
            let load = this.sessions.begin_load();
            let _ = this
                .sessions
                .complete_load(load, vec![child, middle, root, looped]);
            assert_eq!(
                this.session_permission_mode(&key.session),
                PermissionMode::Auto
            );
            assert_eq!(
                this.session_permission_mode(&SessionId::from("ses_loop")),
                PermissionMode::Ask,
                "a parent cycle falls back to ask"
            );
            // The new-session draft shows the Settings default until it chooses.
            this.send_catalog
                .settings
                .as_mut()
                .expect("settings")
                .permission_default_mode = Some(PermissionMode::Auto);
            this.show_new_session(window, cx);
        })
    })
    .expect("window");
    assert_eq!(shown(cx, &workspace), PermissionMode::Auto);
    click_shield(cx, window);
    cx.run_until_parked();
    assert_eq!(shown(cx, &workspace), PermissionMode::Ask);
    assert!(
        writes(&server).is_empty(),
        "a draft's choice is not written"
    );
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn failures_keep_the_last_policy_and_reconnects_reread_it(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_policy");
    let policy = Arc::new(Mutex::new(Policy::default()));
    policy
        .lock()
        .expect("policy")
        .modes
        .insert("ses_policy".into(), json!("auto"));
    let server = policy_server(policy.clone());
    let (window, workspace) = open_policy(cx, &server, &fixture, &key);
    assert_eq!(shown(cx, &workspace), PermissionMode::Auto);

    policy.lock().expect("policy").fail_writes = true;
    click_shield(cx, window);
    settle_writes(cx, &workspace);
    assert_eq!(
        shown(cx, &workspace),
        PermissionMode::Auto,
        "a failed write applies nothing"
    );

    let reads = || count(&server, "GET", "/api/permission-auto-accept");
    let reconnect = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                let generation = this.connection.as_ref().expect("connection").generation;
                this.connection.as_mut().expect("connection").loaded = true;
                this.apply_stream_item(
                    generation,
                    StreamItem::Connected {
                        resumed: true,
                        replay_through: None,
                    },
                    window,
                    cx,
                );
            })
        })
        .expect("window");
    };
    let before = reads();
    policy.lock().expect("policy").fail_reads = true;
    reconnect(cx);
    wait(cx, |cx| {
        reads() == before + 1 && cx.read(|cx| workspace.read(cx).permission_modes.read_idle())
    });
    assert_eq!(
        shown(cx, &workspace),
        PermissionMode::Auto,
        "a failed read keeps the last valid policy"
    );
    {
        let mut policy = policy.lock().expect("policy");
        policy.fail_reads = false;
        policy.modes.insert("ses_policy".into(), json!("ask"));
        policy.revision = 9;
    }
    reconnect(cx);
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).shown_permission_mode() == PermissionMode::Ask)
    });
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn broadcasts_apply_in_revision_order_and_a_runtime_switch_drops_old_completions(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_policy");
    let policy = Arc::new(Mutex::new(Policy {
        revision: 4,
        ..Policy::default()
    }));
    let server = policy_server(policy.clone());
    let (window, workspace) = open_policy(cx, &server, &fixture, &key);
    let broadcast = |cx: &mut TestAppContext, mode, revision| {
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                let generation = this.connection.as_ref().expect("connection").generation;
                this.apply_stream_item(
                    generation,
                    batch(vec![ServerEvent::PermissionPolicy(PolicySnapshot {
                        modes: [(key.session.clone(), mode)].into(),
                        revision,
                    })]),
                    window,
                    cx,
                );
            })
        })
        .expect("window");
    };
    broadcast(cx, PermissionMode::Auto, Some(6));
    assert_eq!(shown(cx, &workspace), PermissionMode::Auto);
    broadcast(cx, PermissionMode::Ask, Some(5));
    broadcast(cx, PermissionMode::Ask, None);
    assert_eq!(
        shown(cx, &workspace),
        PermissionMode::Auto,
        "older and unrevisioned snapshots never replace a newer one"
    );

    policy.lock().expect("policy").hold_writes = true;
    click_shield(cx, window);
    wait(cx, |_| {
        count(&server, "PUT", "/api/permission-auto-accept") == 1
    });
    // The fixture keeps serving the switched-to runtime; failing its reads
    // leaves only the stale write able to load a policy.
    policy.lock().expect("policy").fail_reads = true;
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.switch_runtime("another-runtime".into(), window, cx)
        })
    })
    .expect("window");
    policy.lock().expect("policy").hold_writes = false;
    settle_writes(cx, &workspace);
    cx.read(|cx| {
        let this = workspace.read(cx);
        assert!(
            !this.permission_modes.policy.is_loaded(),
            "a write from the previous runtime does not apply here"
        );
    });
    close(cx, window, &workspace);
}

fn permission(id: &str, session: &str) -> PendingRequest {
    PendingRequest::Permission(PermissionRequest {
        id: id.into(),
        session_id: session.into(),
        action: "shell".into(),
        resources: vec!["printf harmless".into()],
        save: Vec::new(),
        source: None,
        message: None,
        metadata_display: None,
    })
}

#[gpui_kit::test]
fn fresh_requests_stay_hidden_until_the_server_leaves_them_for_the_user(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_policy");
    let policy = Arc::new(Mutex::new(Policy::default()));
    policy
        .lock()
        .expect("policy")
        .modes
        .insert("ses_policy".into(), json!("auto"));
    let server = policy_server(policy);
    let (window, workspace) = open_policy(cx, &server, &fixture, &key);
    let events = |cx: &mut TestAppContext, events: Vec<ServerEvent>| {
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                let generation = this.connection.as_ref().expect("connection").generation;
                this.apply_stream_item(generation, batch(events), window, cx);
            })
        })
        .expect("window");
    };
    let asked = |id: &str| ServerEvent::BlockingRequest {
        directory: Some("/project".into()),
        event: BlockingRequestEvent::Asked(permission(id, "ses_policy")),
    };
    let left = |id: &str| ServerEvent::PermissionLeftForUser {
        session_id: key.session.clone(),
        permission_id: PermissionId::from(id),
        directory: Some("/project".into()),
    };
    let visible = |cx: &mut TestAppContext| {
        cx.read(|cx| {
            workspace
                .read(cx)
                .visible_requests()
                .into_iter()
                .map(|key| match key.id {
                    RequestId::Permission(id) => id.to_string(),
                    RequestId::Form(id) => id.to_string(),
                })
                .collect::<Vec<_>>()
        })
    };

    events(cx, vec![asked("per_held")]);
    assert!(visible(cx).is_empty(), "an auto session's request is held");
    cx.read(|cx| {
        let this = workspace.read(cx);
        assert!(
            this.requests_for_send(&this.mutation_key(&key.session))
                .is_empty(),
            "a send does not reject a request the server is deciding"
        );
    });
    events(cx, vec![left("per_held")]);
    assert_eq!(visible(cx), ["per_held"]);

    events(cx, vec![left("per_early")]);
    events(cx, vec![asked("per_early")]);
    assert_eq!(
        visible(cx).len(),
        2,
        "an early report shows its request at once"
    );

    events(cx, vec![asked("per_answered")]);
    let scope = RequestScope::new(key.runtime.clone(), "/project").expect("scope");
    events(
        cx,
        vec![ServerEvent::BlockingRequest {
            directory: Some("/project".into()),
            event: BlockingRequestEvent::Settled {
                owner: RequestOwner::Session(key.session.clone()),
                id: RequestId::Permission("per_answered".into()),
            },
        }],
    );
    cx.read(|cx| {
        let this = workspace.read(cx);
        let answered = RequestKey {
            scope: scope.clone(),
            owner: RequestOwner::Session(key.session.clone()),
            id: RequestId::Permission("per_answered".into()),
        };
        assert!(
            this.blocking.stores[&scope].get(&answered).is_none(),
            "an answered request leaves the store"
        );
        assert!(!this.request_held(&answered.owner, &answered.id));
    });

    // A gap in the stream may have lost a report: held requests show.
    events(cx, vec![asked("per_gap")]);
    assert_eq!(visible(cx).len(), 2);
    cx.update(|cx| {
        workspace.update(cx, |this, _| {
            let connection = this.connection.as_mut().expect("connection");
            connection.loaded = true;
            connection.stale = true;
        })
    });
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            let generation = this.connection.as_ref().expect("connection").generation;
            this.apply_stream_item(
                generation,
                StreamItem::Connected {
                    resumed: false,
                    replay_through: None,
                },
                window,
                cx,
            );
        })
    })
    .expect("window");
    cx.read(|cx| {
        assert!(!workspace.read(cx).request_held(
            &RequestOwner::Session(key.session.clone()),
            &RequestId::Permission("per_gap".into()),
        ));
    });
    assert_eq!(
        count(&server, "POST", "/api/session/"),
        0,
        "nothing here answers a request"
    );
    close(cx, window, &workspace);
}

/// Applies stream events to `workspace` on its current connection.
fn stream(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    item: StreamItem,
) {
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            let generation = this.connection.as_ref().expect("connection").generation;
            this.apply_stream_item(generation, item, window, cx);
        })
    })
    .expect("window");
}

fn batch(events: Vec<ServerEvent>) -> StreamItem {
    StreamItem::Events {
        events,
        event_id: None,
    }
}

fn asked(id: &str, session: &str, directory: &str) -> StreamItem {
    batch(vec![ServerEvent::BlockingRequest {
        directory: Some(directory.into()),
        event: BlockingRequestEvent::Asked(permission(id, session)),
    }])
}

fn visible_ids(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<String> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .visible_requests()
            .into_iter()
            .map(|key| match key.id {
                RequestId::Permission(id) => id.to_string(),
                RequestId::Form(id) => id.to_string(),
            })
            .collect()
    })
}

#[gpui_kit::test]
fn a_reconnect_after_a_gap_shows_held_requests_even_when_the_replay_resumed(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_policy");
    let policy = Arc::new(Mutex::new(Policy::default()));
    policy
        .lock()
        .expect("policy")
        .modes
        .insert("ses_policy".into(), json!("auto"));
    let server = policy_server(policy);
    let (window, workspace) = open_policy(cx, &server, &fixture, &key);
    stream(
        cx,
        window,
        &workspace,
        asked("per_gap", "ses_policy", "/project"),
    );
    assert!(
        visible_ids(cx, &workspace).is_empty(),
        "an auto session's request is held"
    );
    cx.update(|cx| {
        workspace.update(cx, |this, _| {
            let connection = this.connection.as_mut().expect("connection");
            connection.loaded = true;
            connection.stale = true;
        })
    });
    // The replay never carries `left-for-user`: the UI broadcaster bypasses
    // the hub's replay buffer, so a resumed replay may still have lost it.
    stream(
        cx,
        window,
        &workspace,
        StreamItem::Connected {
            resumed: true,
            replay_through: None,
        },
    );
    assert_eq!(visible_ids(cx, &workspace), ["per_gap"]);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn holds_follow_every_applied_policy_and_safety_net_change(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_policy");
    let policy = Arc::new(Mutex::new(Policy {
        revision: 1,
        safety_net: true,
        ..Policy::default()
    }));
    policy
        .lock()
        .expect("policy")
        .modes
        .insert("ses_policy".into(), json!("auto"));
    let server = policy_server(policy.clone());
    let (window, workspace) = open_policy(cx, &server, &fixture, &key);
    let held = |cx: &mut TestAppContext, id: &str| {
        cx.read(|cx| {
            workspace.read(cx).request_held(
                &RequestOwner::Session(key.session.clone()),
                &RequestId::Permission(id.into()),
            )
        })
    };

    // A broadcast switches the session to Ask while a fresh ask is held:
    // the server answers it `ignored`, with no `left-for-user`.
    stream(
        cx,
        window,
        &workspace,
        asked("per_broadcast", "ses_policy", "/project"),
    );
    assert!(held(cx, "per_broadcast"));
    stream(
        cx,
        window,
        &workspace,
        batch(vec![ServerEvent::PermissionPolicy(PolicySnapshot {
            modes: [(key.session.clone(), PermissionMode::Ask)].into(),
            revision: Some(2),
        })]),
    );
    assert!(!held(cx, "per_broadcast"), "a broadcast to Ask shows it");

    // A read that finds Ask shows it too.
    policy.lock().expect("policy").revision = 2;
    stream(
        cx,
        window,
        &workspace,
        batch(vec![ServerEvent::PermissionPolicy(PolicySnapshot {
            modes: [(key.session.clone(), PermissionMode::Auto)].into(),
            revision: Some(3),
        })]),
    );
    stream(
        cx,
        window,
        &workspace,
        asked("per_read", "ses_policy", "/project"),
    );
    assert!(held(cx, "per_read"));
    {
        let mut policy = policy.lock().expect("policy");
        policy.modes.insert("ses_policy".into(), json!("ask"));
        policy.revision = 4;
    }
    cx.update_window(window, |_, _, cx| {
        workspace.update(cx, |this, cx| this.load_permission_policy(cx))
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).permission_modes.read_idle())
    });
    assert!(!held(cx, "per_read"), "a read that finds Ask shows it");

    // The user's own write to Ask shows it once the server's answer applies.
    policy
        .lock()
        .expect("policy")
        .modes
        .insert("ses_policy".into(), json!("auto"));
    cx.update_window(window, |_, _, cx| {
        workspace.update(cx, |this, cx| this.load_permission_policy(cx))
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).shown_permission_mode() == PermissionMode::Auto)
    });
    stream(
        cx,
        window,
        &workspace,
        asked("per_write", "ses_policy", "/project"),
    );
    assert!(held(cx, "per_write"));
    click_shield(cx, window);
    settle_writes(cx, &workspace);
    assert_eq!(shown(cx, &workspace), PermissionMode::Ask);
    assert!(!held(cx, "per_write"), "a write to Ask shows it");

    // Safety holds only while the safety net runs.
    click_shield(cx, window);
    settle_writes(cx, &workspace);
    assert_eq!(shown(cx, &workspace), PermissionMode::Safety);
    stream(
        cx,
        window,
        &workspace,
        asked("per_safety", "ses_policy", "/project"),
    );
    assert!(held(cx, "per_safety"));
    policy.lock().expect("policy").safety_net = false;
    cx.update_window(window, |_, _, cx| {
        workspace.update(cx, |this, cx| this.load_auto_routing(cx))
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| {
            let this = workspace.read(cx);
            this.send_catalog.routing_read.is_none() && !this.send_catalog.safety_net
        })
    });
    assert!(!held(cx, "per_safety"), "losing the safety net shows it");
    assert_eq!(
        count(&server, "POST", "/api/session/"),
        0,
        "nothing here answers a request"
    );
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn space_sessions_are_never_held_or_shown_on_the_hosts_policy(cx: &mut TestAppContext) {
    const SPACE: &str = "/spaces/0123456789ab/repo";
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_space");
    let policy = Arc::new(Mutex::new(Policy {
        revision: 1,
        ..Policy::default()
    }));
    // A host policy that names the space session must not decide it.
    policy
        .lock()
        .expect("policy")
        .modes
        .insert("ses_space".into(), json!("auto"));
    let server = policy_server(policy);
    let (window, workspace) = open_policy(cx, &server, &fixture, &key);
    cx.update(|cx| {
        workspace.update(cx, |this, _| {
            let mut record = this.sessions.get(&key.session).expect("session").clone();
            record.directory = Some(SPACE.into());
            let load = this.sessions.begin_load();
            let _ = this.sessions.complete_load(load, vec![record]);
        })
    });
    cx.read(|cx| {
        let this = workspace.read(cx);
        assert!(!this.permission_mode_interactive(), "the shield is inert");
        assert_eq!(this.shown_permission_mode(), PermissionMode::Ask);
    });
    click_shield(cx, window);
    cx.run_until_parked();
    assert!(writes(&server).is_empty(), "an inert shield writes nothing");

    stream(
        cx,
        window,
        &workspace,
        asked("per_space", "ses_space", SPACE),
    );
    assert_eq!(
        visible_ids(cx, &workspace),
        ["per_space"],
        "the host's policy never holds a space session's request"
    );

    // A draft created in a space writes its choice to the space; the answer
    // is the space's policy and never lands in the host's.
    let revision = cx.read(|cx| workspace.read(cx).permission_modes.policy.revision());
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.permission_modes
                .creating
                .insert(key.clone(), PermissionMode::Ask);
            let connection = this.connection.as_ref().expect("connection");
            let client = connection.client.clone().expect("client");
            let executor = connection.network.handle();
            this.write_created_permission_mode(&key, SPACE, client, &executor, (window, cx));
        })
    })
    .expect("window");
    settle_writes(cx, &workspace);
    assert_eq!(writes(&server).len(), 1);
    cx.read(|cx| {
        let this = workspace.read(cx);
        assert_eq!(this.permission_modes.policy.revision(), revision);
    });
    close(cx, window, &workspace);
}

fn auto_policy_window(
    cx: &mut TestAppContext,
    fixture: &Fixture,
    key: &SessionKey,
) -> (Server, AnyWindowHandle, Entity<Workspace>) {
    let policy = Arc::new(Mutex::new(Policy {
        revision: 1,
        ..Policy::default()
    }));
    policy
        .lock()
        .expect("policy")
        .modes
        .insert(key.session.to_string(), json!("auto"));
    let server = policy_server(policy);
    let (window, workspace) = open_policy(cx, &server, fixture, key);
    (server, window, workspace)
}

fn is_held(
    cx: &mut TestAppContext,
    workspace: &Entity<Workspace>,
    session: &str,
    permission: &str,
) -> bool {
    cx.read(|cx| {
        workspace.read(cx).request_held(
            &RequestOwner::Session(session.into()),
            &RequestId::Permission(permission.into()),
        )
    })
}

#[gpui_kit::test]
fn a_request_first_replayed_after_a_gap_shows_once_the_reconnect_read_lands(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_policy");
    let (server, window, workspace) = auto_policy_window(cx, &fixture, &key);
    cx.update(|cx| {
        workspace.update(cx, |this, _| {
            let connection = this.connection.as_mut().expect("connection");
            connection.loaded = true;
            connection.stale = true;
        })
    });
    // The server sends `ready` before the replay, so the replayed ask comes
    // after the reconnect, and its `left-for-user` was lost in the gap.
    stream(
        cx,
        window,
        &workspace,
        StreamItem::Connected {
            resumed: true,
            replay_through: None,
        },
    );
    stream(
        cx,
        window,
        &workspace,
        asked("per_replayed", "ses_policy", "/project"),
    );
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).permission_modes.read_idle())
    });
    assert_eq!(visible_ids(cx, &workspace), ["per_replayed"]);

    // After the window, a fresh ask in Auto is held again.
    stream(
        cx,
        window,
        &workspace,
        asked("per_live", "ses_policy", "/project"),
    );
    assert!(is_held(cx, &workspace, "ses_policy", "per_live"));
    assert_eq!(count(&server, "POST", "/api/session/"), 0);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn a_session_move_keeps_unrelated_holds(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_policy");
    let (_server, window, workspace) = auto_policy_window(cx, &fixture, &key);
    stream(
        cx,
        window,
        &workspace,
        asked("per_held", "ses_policy", "/project"),
    );
    assert!(is_held(cx, &workspace, "ses_policy", "per_held"));
    let mut other = SessionRecord::new(SessionId::from("ses_other"), "/project");
    other.time.updated = 1;
    stream(
        cx,
        window,
        &workspace,
        batch(vec![
            ServerEvent::SessionCreated(other),
            ServerEvent::SessionPatched {
                session_id: SessionId::from("ses_other"),
                patch: openchamber_native_core::session::SessionPatch {
                    directory: Some("/elsewhere".into()),
                    updated: Some(2),
                    ..Default::default()
                },
            },
        ]),
    );
    assert!(
        is_held(cx, &workspace, "ses_policy", "per_held"),
        "a move is no gap: the report still arrives live"
    );
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn a_new_subagents_first_request_inherits_its_parents_mode_within_one_batch(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_policy");
    let (_server, window, workspace) = auto_policy_window(cx, &fixture, &key);
    let mut child = SessionRecord::new(SessionId::from("ses_child"), "/project");
    child.parent_id = Some(key.session.clone());
    let StreamItem::Events { mut events, .. } = asked("per_child", "ses_child", "/project") else {
        unreachable!("asked builds events");
    };
    events.insert(0, ServerEvent::SessionCreated(child));
    stream(cx, window, &workspace, batch(events));
    assert!(
        is_held(cx, &workspace, "ses_child", "per_child"),
        "the child's request is decided on its parent's Auto, never flashed"
    );
    close(cx, window, &workspace);
}

/// Marks the connection as having missed events, as a dropped stream does.
fn go_stale(cx: &mut TestAppContext, workspace: &Entity<Workspace>) {
    cx.update(|cx| {
        workspace.update(cx, |this, _| {
            let connection = this.connection.as_mut().expect("connection");
            connection.loaded = true;
            connection.stale = true;
        })
    });
}

fn replayed(id: &str, session: &str, event_id: &str) -> StreamItem {
    let StreamItem::Events { events, .. } = asked(id, session, "/project") else {
        unreachable!("asked builds events");
    };
    StreamItem::Events {
        events,
        event_id: Some(event_id.into()),
    }
}

#[gpui_kit::test]
fn the_replay_window_outlasts_a_policy_read_that_lands_mid_replay(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_policy");
    let (server, window, workspace) = auto_policy_window(cx, &fixture, &key);
    go_stale(cx, &workspace);
    stream(
        cx,
        window,
        &workspace,
        StreamItem::Connected {
            resumed: true,
            replay_through: Some("e3".into()),
        },
    );
    stream(
        cx,
        window,
        &workspace,
        replayed("per_first", "ses_policy", "e2"),
    );
    // The policy read settles between the two replayed batches.
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).permission_modes.read_idle())
    });
    assert!(
        is_held(cx, &workspace, "ses_policy", "per_first"),
        "the replay is not over yet"
    );
    stream(
        cx,
        window,
        &workspace,
        replayed("per_later", "ses_policy", "e3"),
    );
    assert!(!is_held(cx, &workspace, "ses_policy", "per_first"));
    assert!(
        !is_held(cx, &workspace, "ses_policy", "per_later"),
        "a request replayed after the read still shows"
    );
    assert_eq!(visible_ids(cx, &workspace).len(), 2);

    // Live asks after the replay are held again.
    stream(
        cx,
        window,
        &workspace,
        replayed("per_live", "ses_policy", "e4"),
    );
    assert!(is_held(cx, &workspace, "ses_policy", "per_live"));
    assert_eq!(count(&server, "POST", "/api/session/"), 0);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn the_last_replayed_event_waits_for_the_policy_read(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_policy");
    let (_server, window, workspace) = auto_policy_window(cx, &fixture, &key);
    go_stale(cx, &workspace);
    stream(
        cx,
        window,
        &workspace,
        StreamItem::Connected {
            resumed: true,
            replay_through: Some("e3".into()),
        },
    );
    // The whole replay lands before the read settles.
    stream(
        cx,
        window,
        &workspace,
        replayed("per_replayed", "ses_policy", "e3"),
    );
    assert!(is_held(cx, &workspace, "ses_policy", "per_replayed"));
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).permission_modes.read_idle())
    });
    assert!(!is_held(cx, &workspace, "ses_policy", "per_replayed"));
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn an_empty_or_reset_replay_closes_on_the_policy_read_alone(cx: &mut TestAppContext) {
    for resumed in [true, false] {
        let fixture = Fixture::new();
        let key = tests::key("policy-runtime", "ses_policy");
        let (_server, window, workspace) = auto_policy_window(cx, &fixture, &key);
        go_stale(cx, &workspace);
        // `resumed: false` is what a `replayReset` ready becomes.
        stream(
            cx,
            window,
            &workspace,
            StreamItem::Connected {
                resumed,
                replay_through: None,
            },
        );
        stream(
            cx,
            window,
            &workspace,
            replayed("per_gap", "ses_policy", "e1"),
        );
        assert!(
            is_held(cx, &workspace, "ses_policy", "per_gap"),
            "resumed {resumed}"
        );
        wait(cx, |cx| {
            cx.read(|cx| workspace.read(cx).permission_modes.read_idle())
        });
        assert!(
            !is_held(cx, &workspace, "ses_policy", "per_gap"),
            "resumed {resumed}: the read alone closes the window"
        );
        close(cx, window, &workspace);
    }
}

#[gpui_kit::test]
fn a_second_gap_supersedes_an_open_replay_window(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("policy-runtime", "ses_policy");
    let (_server, window, workspace) = auto_policy_window(cx, &fixture, &key);
    let connect = |cx: &mut TestAppContext, through: &str| {
        go_stale(cx, &workspace);
        stream(
            cx,
            window,
            &workspace,
            StreamItem::Connected {
                resumed: true,
                replay_through: Some(through.into()),
            },
        );
    };
    connect(cx, "e3");
    stream(
        cx,
        window,
        &workspace,
        replayed("per_first", "ses_policy", "e2"),
    );
    connect(cx, "e9");
    assert!(
        !is_held(cx, &workspace, "ses_policy", "per_first"),
        "the second gap shows what the first window held"
    );
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).permission_modes.read_idle())
    });
    stream(
        cx,
        window,
        &workspace,
        replayed("per_second", "ses_policy", "e3"),
    );
    assert!(
        is_held(cx, &workspace, "ses_policy", "per_second"),
        "the first window's last event no longer ends the replay"
    );
    stream(
        cx,
        window,
        &workspace,
        replayed("per_last", "ses_policy", "e9"),
    );
    assert!(!is_held(cx, &workspace, "ses_policy", "per_second"));
    assert!(!is_held(cx, &workspace, "ses_policy", "per_last"));
    close(cx, window, &workspace);
}
