//! End-to-end state tests with the real journal and a controlled HTTP server.

use openchamber_native_core::ids::ProjectId;
use openchamber_native_core::ownership::ConfiguredProject;
use serde_json::json;

use crate::create_attempt::CreationPhase;
use crate::send_attempt::SendOrigin;

use super::*;

#[derive(Clone, Copy)]
enum Outcome {
    Success,
    Lost,
    WrongId,
    WrongDirectory,
    Rejected,
}

fn creation_server(journal: PathBuf, outcome: Outcome, hold: bool) -> (Server, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = Endpoint::parse(&format!(
        "http://{}",
        listener.local_addr().expect("address")
    ))
    .expect("endpoint");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let release_prompt = Arc::new(AtomicBool::new(!hold));
    let missing = Arc::new(AtomicBool::new(false));
    let recorded = requests.clone();
    let stopped = stop.clone();
    let release = release_prompt.clone();
    let absent = missing.clone();
    let worker = thread::spawn(move || {
        let mut session = None;
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
            let mut request = read_request(&mut stream);
            let create = request.method == "POST" && request.path == "/api/session";
            if request.method == "POST" {
                request.journal = fs::read(&journal)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok());
            }
            if create && !matches!(outcome, Outcome::Rejected) {
                let body: Value = serde_json::from_slice(&request.body).expect("create body");
                session = Some(json!({"data": {
                    "id": body["id"], "projectID": "global", "location": body["location"],
                    "model": body.get("model").cloned().unwrap_or(json!({"providerID":"provider","id":"model"})),
                    "agent": body.get("agent").cloned().unwrap_or(json!("agent")),
                    "title":"Created session", "time":{"created":1793209852000_i64,"updated":1793209852000_i64},
                    "cost":0,"tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}
                }}));
            }
            let (status, body) = if create {
                if matches!(outcome, Outcome::Rejected) {
                    (400, json!({}))
                } else {
                    let mut body = session.clone().expect("session");
                    match outcome {
                        Outcome::WrongId => body["data"]["id"] = "ses_unrelated".into(),
                        Outcome::WrongDirectory => {
                            body["data"]["location"]["directory"] = "/unrelated".into()
                        }
                        _ => {}
                    }
                    (200, body)
                }
            } else if request.method == "POST" && !request.path.ends_with("/prompt") {
                (204, json!({}))
            } else if request.path.ends_with("/inbox") || request.path.contains("/message?") {
                (200, json!({"data":[]}))
            } else if request.method == "GET"
                && session.as_ref().is_some_and(|body| {
                    request.path
                        == format!("/api/session/{}", body["data"]["id"].as_str().expect("id"))
                })
                && !absent.load(Ordering::Acquire)
            {
                (200, session.clone().expect("session"))
            } else if request.path.ends_with("/prompt") {
                (200, json!({}))
            } else {
                (404, json!({}))
            };
            recorded.lock().expect("requests").push(request);
            if create {
                while !release.load(Ordering::Acquire) && !stopped.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                if matches!(outcome, Outcome::Lost) {
                    continue;
                }
            }
            let body = if status == 204 {
                String::new()
            } else {
                body.to_string()
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (
        Server {
            endpoint,
            requests,
            stop,
            release_prompt,
            worker: Some(worker),
        },
        missing,
    )
}

fn prepare(
    workspace: &mut Workspace,
    key: &SessionKey,
    window: &mut gpui_kit::Window,
    cx: &mut gpui_kit::Context<Workspace>,
) {
    configure_send(workspace, key, window, cx);
    workspace.topology.projects = vec![ConfiguredProject {
        id: ProjectId::from("registered"),
        path: "/project".into(),
    }];
    workspace.show_new_session(window, cx);
    workspace.choose_draft_project("/project", window, cx);
    workspace.composer.update(cx, |composer, cx| {
        composer.set_value("first creation prompt", window, cx)
    });
    assert!(workspace.can_create_connected());
}

fn create_count(server: &Server) -> usize {
    server
        .requests
        .lock()
        .expect("requests")
        .iter()
        .filter(|request| request.method == "POST" && request.path == "/api/session")
        .count()
}

#[gpui_kit::test]
fn creation_and_binding_save_failures_never_dispatch_without_durable_state(
    cx: &mut TestAppContext,
) {
    for fail_binding in [false, true] {
        let fixture = Fixture::new();
        let key = tests::key("create-runtime", "ses_existing");
        let (server, _) = creation_server(fixture.journal(), Outcome::Success, fail_binding);
        let (window, workspace) = open(cx, &server, &fixture, &key);
        ready(cx, &workspace);
        let profile = fixture.journal().parent().expect("profile").to_owned();
        let parked = fixture.0.join("parked-profile");
        if !fail_binding {
            fs::rename(&profile, &parked).expect("move owned test profile");
        }
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                prepare(workspace, &key, window, cx);
                workspace.submit(window, cx);
            })
        })
        .expect("window");
        if fail_binding {
            wait(cx, |_| create_count(&server) == 1);
            fs::rename(&profile, &parked).expect("move owned test profile");
            server.release_prompt.store(true, Ordering::Release);
        }
        wait(cx, |cx| {
            cx.read(|cx| {
                let workspace = workspace.read(cx);
                !workspace.persistence_ready()
                    && workspace.creation_tasks.is_empty()
                    && workspace.mutation_tasks.is_empty()
            })
        });
        assert_eq!(server.prompt_count(), 0);
        assert_eq!(create_count(&server), usize::from(fail_binding));
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            if fail_binding {
                let attempt = workspace
                    .send_attempts
                    .iter()
                    .next()
                    .expect("bound attempt")
                    .1;
                assert_eq!(attempt.origin, SendOrigin::Created);
                assert_eq!(attempt.admission, Admission::Uncertain);
                assert_eq!(attempt.prompt.text, "first creation prompt");
            } else {
                let attempt = workspace
                    .creations
                    .iter()
                    .next()
                    .expect("creation retained")
                    .1;
                assert_eq!(attempt.phase, CreationPhase::Uncertain);
                assert_eq!(attempt.prompt.text, "first creation prompt");
            }
        });
        fs::rename(&parked, &profile).expect("restore test profile");
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.retry_persistence(window, cx))
        })
        .expect("window");
        ready(cx, &workspace);
        close(cx, window, &workspace);
        let (_, restored) = MutationJournal::open(&fixture.journal()).expect("restored journal");
        let LoadedJournal::Present(snapshot) = restored else {
            panic!("saved snapshot")
        };
        assert_eq!(snapshot.creations.len(), usize::from(!fail_binding));
        assert_eq!(snapshot.attempts.len(), usize::from(fail_binding));
    }
}

#[gpui_kit::test]
fn creation_binds_durably_before_prompt_and_skips_redundant_agent_switch(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("create-runtime", "ses_existing");
    let (server, _) = creation_server(fixture.journal(), Outcome::Success, false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            prepare(workspace, &key, window, cx);
            workspace.submit(window, cx);
            workspace.submit(window, cx);
            let created = workspace
                .creations
                .iter()
                .next()
                .expect("created identity")
                .0
                .clone();
            let owner = openchamber_native_core::draft::DraftKey::new(
                created.runtime,
                "/project",
                openchamber_native_core::draft::DraftTarget::Session(created.session),
            )
            .expect("session owner");
            workspace.drafts.save(
                owner,
                openchamber_native_core::draft::Draft::new("first creation prompt"),
            );
        })
    })
    .expect("window");
    wait(cx, |cx| {
        server.prompt_count() == 1 && cx.read(|cx| workspace.read(cx).mutation_tasks.is_empty())
    });
    {
        let requests = server.requests.lock().expect("requests");
        let create = requests
            .iter()
            .find(|request| request.path == "/api/session")
            .expect("create");
        let body: Value = serde_json::from_slice(&create.body).expect("body");
        let journal = create.journal.as_ref().expect("durable before POST");
        assert_eq!(journal["creations"][0]["session"], body["id"]);
        assert!(body.get("parentID").is_none());
        let prompt = requests
            .iter()
            .find(|request| request.path.ends_with("/prompt"))
            .expect("prompt");
        let journal = prompt.journal.as_ref().expect("durable binding");
        assert!(
            journal["creations"]
                .as_array()
                .expect("creations")
                .is_empty()
        );
        assert_eq!(journal["attempts"][0]["origin"], "created");
        assert_eq!(journal["attempts"][0]["session"], body["id"]);
        assert!(
            journal["drafts"]
                .as_array()
                .expect("drafts")
                .iter()
                .any(|draft| draft["text"] == "first creation prompt"),
            "newer identical destination draft survives binding"
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.path.ends_with("/agent")),
            "same agent must not add a visible switch notice"
        );
    }
    assert_eq!(create_count(&server), 1);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn lost_creation_restarts_and_check_never_sends_before_explicit_retry(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("create-runtime", "ses_existing");
    let (server, _) = creation_server(fixture.journal(), Outcome::Lost, false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            prepare(workspace, &key, window, cx);
            workspace.submit(window, cx);
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace
                .read(cx)
                .creations
                .iter()
                .any(|(_, attempt)| attempt.phase == CreationPhase::Confirmed)
        })
    });
    assert_eq!(server.prompt_count(), 0);
    let captured = cx.read(|cx| {
        workspace
            .read(cx)
            .creations
            .iter()
            .next()
            .expect("retained")
            .0
            .clone()
    });
    close(cx, window, &workspace);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.check_recovery(&captured, window, cx)
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).creation_tasks.is_empty())
    });
    assert_eq!(server.prompt_count(), 0);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.retry_recovery(&captured, window, cx)
        })
    })
    .expect("window");
    wait(cx, |_| server.prompt_count() == 1);
    assert_eq!(
        create_count(&server),
        1,
        "confirmed session is never recreated"
    );
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn closing_during_creation_restores_unknown_identity_without_resubmitting(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("create-runtime", "ses_existing");
    let (server, _) = creation_server(fixture.journal(), Outcome::Success, true);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            prepare(workspace, &key, window, cx);
            workspace.submit(window, cx);
        })
    })
    .expect("window");
    wait(cx, |_| create_count(&server) == 1);
    let captured = cx.read(|cx| {
        workspace
            .read(cx)
            .creations
            .iter()
            .next()
            .expect("captured")
            .0
            .clone()
    });
    close(cx, window, &workspace);
    server.release_prompt.store(true, Ordering::Release);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.read(|cx| {
        let attempt = workspace
            .read(cx)
            .creations
            .get(&captured)
            .expect("saved identity");
        assert_eq!(attempt.phase, CreationPhase::Uncertain);
        assert!(attempt.navigation.is_none());
    });
    assert_eq!(create_count(&server), 1);
    assert_eq!(server.prompt_count(), 0);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.check_recovery(&captured, window, cx)
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace
                .read(cx)
                .creations
                .get(&captured)
                .is_some_and(|attempt| attempt.phase == CreationPhase::Confirmed)
        })
    });
    assert_eq!(server.prompt_count(), 0);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn returning_to_same_draft_does_not_let_late_creation_steal_selection(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("create-runtime", "ses_existing");
    let (server, _) = creation_server(fixture.journal(), Outcome::Success, true);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            prepare(workspace, &key, window, cx);
            workspace.submit(window, cx);
            workspace.select_session(key.session.clone(), window, cx);
            workspace.show_new_session(window, cx);
        })
    })
    .expect("window");
    wait(cx, |_| create_count(&server) == 1);
    server.release_prompt.store(true, Ordering::Release);
    wait(cx, |_| server.prompt_count() == 1);
    cx.read(|cx| assert!(workspace.read(cx).selected.is_none()));
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn newer_identical_creation_typing_and_runtime_selection_survive_late_success(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("create-runtime", "ses_existing");
    let (server, _) = creation_server(fixture.journal(), Outcome::Success, true);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            prepare(workspace, &key, window, cx);
            workspace.submit(window, cx);
        })
    })
    .expect("window");
    wait(cx, |_| create_count(&server) == 1);
    let source = cx.read(|cx| workspace.read(cx).composer_key.clone().expect("source"));
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("first creation prompt", window, cx)
            });
            workspace.save_composer_draft(cx);
            workspace.switch_runtime(RuntimeId::from("other-runtime"), window, cx);
            workspace.composer.update(cx, |composer, cx| {
                composer.set_value("other runtime typing", window, cx)
            });
        })
    })
    .expect("window");
    server.release_prompt.store(true, Ordering::Release);
    wait(cx, |_| server.prompt_count() == 1);
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        assert_eq!(workspace.runtime.as_str(), "other-runtime");
        assert!(workspace.selected.is_none());
        assert_eq!(
            workspace.composer.read(cx).value().as_ref(),
            "other runtime typing"
        );
        assert_eq!(
            workspace.drafts.get(&source).expect("newer source").text,
            "first creation prompt"
        );
    });
    close(cx, window, &workspace);
    let (_, loaded) = MutationJournal::open(&fixture.journal()).expect("journal");
    let LoadedJournal::Present(snapshot) = loaded else {
        panic!("snapshot")
    };
    assert_eq!(snapshot.attempts[0].0.runtime.as_str(), "create-runtime");
    assert!(
        snapshot
            .drafts
            .iter()
            .any(|(owner, draft)| owner == &source && draft.text == "first creation prompt")
    );
}

#[gpui_kit::test]
fn creation_mismatch_and_explicit_deletion_block_first_prompt(cx: &mut TestAppContext) {
    for outcome in [Outcome::WrongId, Outcome::WrongDirectory, Outcome::Success] {
        let fixture = Fixture::new();
        let key = tests::key("create-runtime", "ses_existing");
        let (server, _) = creation_server(fixture.journal(), outcome, true);
        let (window, workspace) = open(cx, &server, &fixture, &key);
        ready(cx, &workspace);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                prepare(workspace, &key, window, cx);
                workspace.submit(window, cx);
            })
        })
        .expect("window");
        wait(cx, |_| create_count(&server) == 1);
        if matches!(outcome, Outcome::Success) {
            cx.update_window(window, |_, _, cx| {
                workspace.update(cx, |workspace, cx| {
                    let key = workspace
                        .creations
                        .iter()
                        .next()
                        .expect("attempt")
                        .0
                        .clone();
                    workspace.mark_creation_deleted(&key, cx);
                })
            })
            .expect("window");
        }
        server.release_prompt.store(true, Ordering::Release);
        wait(cx, |cx| {
            cx.read(|cx| workspace.read(cx).creation_tasks.is_empty())
        });
        assert_eq!(server.prompt_count(), 0);
        cx.read(|cx| {
            assert_eq!(
                workspace
                    .read(cx)
                    .creations
                    .iter()
                    .next()
                    .expect("retained")
                    .1
                    .phase,
                if matches!(outcome, Outcome::Success) {
                    CreationPhase::Deleted
                } else {
                    CreationPhase::Mismatch
                }
            )
        });
        close(cx, window, &workspace);
    }
}

#[gpui_kit::test]
fn confirmed_creation_404_is_terminal_and_rejected_creation_retries_same_identity(
    cx: &mut TestAppContext,
) {
    for outcome in [Outcome::Lost, Outcome::Rejected] {
        let fixture = Fixture::new();
        let key = tests::key("create-runtime", "ses_existing");
        let (server, missing) = creation_server(fixture.journal(), outcome, false);
        let (window, workspace) = open(cx, &server, &fixture, &key);
        ready(cx, &workspace);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                prepare(workspace, &key, window, cx);
                workspace.submit(window, cx);
            })
        })
        .expect("window");
        wait(cx, |cx| {
            create_count(&server) == 1 && cx.read(|cx| workspace.read(cx).creation_tasks.is_empty())
        });
        let captured = cx.read(|cx| {
            workspace
                .read(cx)
                .creations
                .iter()
                .next()
                .expect("retained")
                .0
                .clone()
        });
        missing.store(true, Ordering::Release);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.retry_recovery(&captured, window, cx)
            })
        })
        .expect("window");
        wait(cx, |cx| {
            cx.read(|cx| workspace.read(cx).creation_tasks.is_empty())
        });
        if matches!(outcome, Outcome::Lost) {
            assert_eq!(create_count(&server), 1);
            cx.read(|cx| {
                assert_eq!(
                    workspace
                        .read(cx)
                        .creations
                        .get(&captured)
                        .expect("retained")
                        .phase,
                    CreationPhase::Deleted
                )
            });
        } else {
            assert_eq!(create_count(&server), 2);
            let requests = server.requests.lock().expect("requests");
            let posts: Vec<_> = requests
                .iter()
                .filter(|request| request.path == "/api/session" && request.method == "POST")
                .collect();
            assert_eq!(posts[0].body, posts[1].body);
        }
        assert_eq!(server.prompt_count(), 0);
        close(cx, window, &workspace);
    }
}
