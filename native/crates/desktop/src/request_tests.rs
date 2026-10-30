//! Headless dock actions against a scripted HTTP server, with captured request ownership.

use super::*;
use crate::blocking_requests::RequestReply;
use gpui_kit::ElementId;
use openchamber_native_core::form::{
    FormField, FormFieldKind, FormNumber, FormOption, FormRequest, FormValue,
};
use openchamber_native_core::pending_requests::{
    PendingRequest, RequestId, RequestKey, ResponseState,
};
use openchamber_native_core::permission::{PermissionReply, PermissionRequest};
use openchamber_native_core::request_owner::{RequestOwner, RequestScope};
use openchamber_native_core::session_index::SessionChange;
use openchamber_native_transport::event::BlockingRequestEvent;
use serde_json::json;

fn permission(owner: &str) -> PendingRequest {
    PendingRequest::Permission(PermissionRequest {
        id: "per_dock".into(),
        session_id: owner.into(),
        action: "shell".into(),
        resources: vec!["printf harmless".into()],
        save: vec!["printf *".into()],
        source: None,
        message: None,
        metadata_display: None,
    })
}

fn field(key: &str, kind: FormFieldKind) -> FormField {
    FormField {
        key: key.into(),
        title: None,
        description: None,
        required: true,
        hidden: false,
        when: Vec::new(),
        kind,
    }
}

pub(super) fn form(owner: RequestOwner) -> PendingRequest {
    PendingRequest::Form(FormRequest {
        id: "frm_dock".into(),
        owner,
        title: "Fixture question".into(),
        metadata_display: None,
        fields: vec![
            field(
                "text",
                FormFieldKind::String {
                    format: None,
                    min_length: None,
                    max_length: None,
                    pattern: Some(".*".into()),
                    placeholder: None,
                    default: Some("ready".into()),
                    options: None,
                    custom: true,
                },
            ),
            field("boolean", FormFieldKind::Boolean { default: None }),
            FormField {
                required: false,
                ..field(
                    "external",
                    FormFieldKind::External {
                        url: "https://example.test/ack".into(),
                    },
                )
            },
        ],
    })
}

pub(super) fn ask(
    workspace: &mut Workspace,
    request: PendingRequest,
    window: &mut gpui_kit::Window,
    cx: &mut gpui_kit::Context<Workspace>,
) -> RequestKey {
    let scope = RequestScope::new(workspace.runtime.clone(), "/project").expect("scope");
    let key = request.key(&scope);
    workspace.blocking_event(
        Some("/project".into()),
        BlockingRequestEvent::Asked(request),
        window,
        cx,
    );
    key
}

fn request_server(owner: &str, lose: bool, hold: bool) -> Server {
    request_server_with_read_gate(owner, lose, hold, false)
}

fn request_server_with_read_gate(owner: &str, lose: bool, hold: bool, hold_read: bool) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = Endpoint::parse(&format!(
        "http://{}",
        listener.local_addr().expect("address")
    ))
    .expect("endpoint");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let release_prompt = Arc::new(AtomicBool::new(!hold));
    let recorded = requests.clone();
    let stopped = stop.clone();
    let release = release_prompt.clone();
    let owner = owner.to_owned();
    let worker = thread::spawn(move || {
        let mut lose = lose;
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
            let request = read_request(&mut stream);
            let exact_read = request.method == "GET"
                && (request.path.contains("/permission/per_dock")
                    || request.path.ends_with("/inbox"));
            let mutation = request.path.contains("/permission/") && request.method == "POST"
                || request.path.contains("/form/")
                    && (request.method == "POST" || request.method == "DELETE");
            let (status, body) = if mutation {
                (204, json!(null))
            } else if request.path.contains("/permission/per_dock") {
                (
                    200,
                    json!({"data":{"id":"per_dock","sessionID":owner,"action":"shell","resources":["printf harmless"],"save":["printf *"]}}),
                )
            } else if request.path.ends_with("/inbox") || request.path.contains("/message?") {
                (200, json!({"data":[]}))
            } else if request.path.ends_with("/prompt") {
                (200, json!({}))
            } else if request.method == "POST" {
                (204, json!(null))
            } else {
                (404, json!({}))
            };
            recorded.lock().expect("requests").push(request);
            if hold_read && exact_read {
                while !release.load(Ordering::Acquire) && !stopped.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            if mutation {
                while !release.load(Ordering::Acquire) && !stopped.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                if lose {
                    lose = false;
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
    Server {
        endpoint,
        requests,
        stop,
        release_prompt,
        worker: Some(worker),
    }
}

fn mutations(server: &Server) -> usize {
    server
        .requests
        .lock()
        .expect("requests")
        .iter()
        .filter(|request| {
            request.path.contains("/permission/") && request.method == "POST"
                || request.path.contains("/form/")
                    && (request.method == "POST" || request.method == "DELETE")
        })
        .count()
}

#[gpui_kit::test]
fn retry_observation_does_not_cancel_permission_arriving_after_activation(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("retry-activation", "ses_parent");
    fixture.seed(&key);
    let server = request_server_with_read_gate("ses_parent", false, true, true);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            this.retry_recovery(&key, &this.recovery_message(&key, cx), window, cx);
        })
    })
    .expect("window");
    wait(cx, |_| {
        server
            .requests
            .lock()
            .expect("requests")
            .iter()
            .any(|request| request.path.ends_with("/inbox"))
    });
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            ask(this, permission("ses_parent"), window, cx);
        })
    })
    .expect("window");
    server.release_prompt.store(true, Ordering::Release);
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace.read(cx).recovery_items(cx).iter().any(|row| {
                row.phase == crate::recovery_view::RecoveryPhase::NotObserved && row.can_retry
            })
        })
    });
    assert_eq!(
        mutations(&server),
        0,
        "late permission was not captured by the retry activation"
    );
    assert_eq!(
        server.prompt_count(),
        0,
        "late permission blocks the captured prompt"
    );
    cx.read(|cx| {
        let this = workspace.read(cx);
        assert_eq!(this.visible_requests().len(), 1);
        assert_eq!(
            this.send_attempts
                .session(&key)
                .next()
                .expect("retained prompt")
                .prompt
                .text,
            tests::attempt(&key).prompt.text
        );
    });
    // A new explicit retry captures the now-visible permission independently.
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.retry_recovery(&key, &this.recovery_message(&key, cx), window, cx);
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| {
            server.prompt_count() == 1 && !workspace.read(cx).send_attempts.is_active(&key)
        })
    });
    assert_eq!(
        mutations(&server),
        1,
        "only the new activation may cancel the permission"
    );
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn superseding_send_runtime_switch_retains_late_blocker_and_prompt(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("supersede-switch", "ses_parent");
    let server = request_server("ses_parent", false, true);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            ask(this, permission("ses_parent"), window, cx);
            this.composer.update(cx, |input, cx| {
                input.set_value("retained prompt", window, cx)
            });
            this.submit_connected(window, cx);
        })
    })
    .expect("window");
    wait(cx, |_| mutations(&server) == 1);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            ask(this, form(RequestOwner::Location), window, cx);
            this.switch_runtime(RuntimeId::from("other-runtime"), window, cx);
        })
    })
    .expect("window");
    server.release_prompt.store(true, Ordering::Release);
    wait(cx, |cx| {
        cx.read(|cx| !workspace.read(cx).send_attempts.is_active(&key))
    });
    assert_eq!(
        server.prompt_count(),
        0,
        "the old runtime's blocker must prevent dispatch"
    );
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            assert!(this.blocking.stores.iter().any(|(scope, store)| {
                scope.runtime() == &key.runtime
                    && store
                        .entries()
                        .any(|(_, entry)| matches!(entry.request, PendingRequest::Form(_)))
            }));
            this.switch_runtime(key.runtime.clone(), window, cx);
            configure_send(this, &key, window, cx);
            assert_eq!(this.composer.read(cx).value().as_ref(), "retained prompt");
        })
    })
    .expect("window");
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn invalidated_exact_hint_read_keeps_child_discovery_until_fresh_lookup(cx: &mut TestAppContext) {
    use openchamber_native_transport::client::PendingRequestStub;
    let fixture = Fixture::new();
    let key = tests::key("hint-invalidation", "ses_parent");
    let server = request_server_with_read_gate("ses_child", false, true, true);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            let mut child = SessionRecord::new("ses_child", "/child");
            child.parent_id = Some(key.session.clone());
            let _ = this.sessions.apply([SessionChange::Created(child)]);
            this.blocking.seeds.insert(
                key.runtime.clone(),
                vec![PendingRequestStub {
                    owner: RequestOwner::Session("ses_child".into()),
                    id: RequestId::Permission("per_dock".into()),
                }],
            );
            this.refresh_blocking_requests(window, cx);
        })
    })
    .expect("window");
    wait(cx, |_| {
        server
            .requests
            .lock()
            .expect("requests")
            .iter()
            .any(|request| request.path.contains("/permission/per_dock"))
    });
    cx.update_window(window, |_, _, cx| {
        workspace.update(cx, |this, _| this.invalidate_blocking(Some("/child")))
    })
    .expect("window");
    server.release_prompt.store(true, Ordering::Release);
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace
                .read(cx)
                .blocking
                .seeds
                .get(&key.runtime)
                .is_some_and(|seeds| seeds.is_empty())
                || !workspace.read(cx).blocking.errors.is_empty()
        })
    });
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            assert_eq!(
                this.blocking.seeds[&key.runtime].len(),
                1,
                "a rejected exact result cannot consume its discovery hint"
            );
            this.refresh_blocking_requests(window, cx);
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).active_request().is_some())
    });
    cx.read(|cx| {
        let this = workspace.read(cx);
        let request = this
            .active_request()
            .expect("a fresh lookup must recover the child request");
        assert_eq!(request.scope.directory(), "/child");
        assert!(this.request_actionable(&request));
    });
    assert_eq!(
        server
            .requests
            .lock()
            .expect("requests")
            .iter()
            .filter(|request| request.path.contains("/permission/per_dock"))
            .count(),
        2,
        "invalidation requires a fresh exact lookup"
    );
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn permission_dock_double_activation_routes_to_child_across_switch(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("dock-runtime", "ses_parent");
    let server = request_server("ses_child", false, true);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            let mut child = SessionRecord::new("ses_child", "/project");
            child.parent_id = Some(key.session.clone());
            let _ = this.sessions.apply([SessionChange::Created(child)]);
            ask(
                this,
                form(RequestOwner::Session(key.session.clone())),
                window,
                cx,
            );
            ask(this, permission("ses_child"), window, cx);
            assert!(matches!(
                this.active_request().expect("permission wins").id,
                RequestId::Permission(_)
            ));
        });
        window.render_frame(cx);
        window.click(ElementId::Name("permission-per_dock-once".into()), cx);
        window.click(ElementId::Name("permission-per_dock-once".into()), cx);
        workspace.update(cx, |this, cx| {
            this.show_new_session(window, cx);
            assert!(this.active_request().is_none());
        });
    })
    .expect("window");
    wait(cx, |_| mutations(&server) == 1);
    server.release_prompt.store(true, Ordering::Release);
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace.read(cx).blocking.stores.values().all(|store| {
                store
                    .entries()
                    .all(|(_, entry)| !matches!(entry.request, PendingRequest::Permission(_)))
            })
        })
    });
    let requests = server.requests.lock().expect("requests");
    let request = requests
        .iter()
        .find(|request| request.method == "POST" && request.path.contains("/permission/"))
        .expect("reply");
    assert!(
        request
            .path
            .contains("/session/ses_child/permission/per_dock/reply")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&request.body).expect("body"),
        json!({"decision":"once"})
    );
    drop(requests);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn host_pending_seed_fetches_only_relevant_full_requests(cx: &mut TestAppContext) {
    use openchamber_native_transport::client::PendingRequestStub;
    let fixture = Fixture::new();
    let key = tests::key("seed-runtime", "ses_parent");
    let server = request_server("ses_parent", false, false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            let mut dormant = SessionRecord::new("ses_dormant_child", "/dormant");
            dormant.parent_id = Some(key.session.clone());
            let _ = this.sessions.apply([SessionChange::Created(dormant)]);
            let _ = this
                .sessions
                .apply([SessionChange::Created(SessionRecord::new(
                    "ses_elsewhere",
                    "/unopened",
                ))]);
            this.blocking.seeds.insert(
                this.runtime.clone(),
                vec![
                    PendingRequestStub {
                        owner: RequestOwner::Location,
                        id: RequestId::Form("frm_unknown_directory".into()),
                    },
                    PendingRequestStub {
                        owner: RequestOwner::Session(key.session.clone()),
                        id: RequestId::Permission("per_dock".into()),
                    },
                    PendingRequestStub {
                        owner: RequestOwner::Session("ses_elsewhere".into()),
                        id: RequestId::Permission("per_other".into()),
                    },
                ],
            );
            this.refresh_blocking_requests(window, cx);
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace
                .read(cx)
                .active_request()
                .is_some_and(|key| workspace.read(cx).request_actionable(&key))
        })
    });
    cx.read(|cx| {
        assert!(
            workspace
                .read(cx)
                .blocking
                .stores
                .keys()
                .all(|scope| scope.directory() == "/project")
        )
    });
    assert!(
        server
            .requests
            .lock()
            .expect("requests")
            .iter()
            .all(|request| !request.path.contains("ses_elsewhere")
                && !request.path.contains("/session/global/"))
    );
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn permission_action_is_reachable_by_tab_and_space(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("keyboard-request-runtime", "ses_parent");
    let server = request_server("ses_parent", false, false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            ask(this, permission("ses_parent"), window, cx);
        });
        window.render_frame(cx);
        let mut found = false;
        for _ in 0..120 {
            window.press("tab", cx);
            window.render_frame(cx);
            if window
                .find(ElementId::Name("permission-per_dock-once".into()))
                .focused()
                == Some(true)
            {
                window.press("space", cx);
                found = true;
                break;
            }
        }
        assert!(found, "permission buttons participate in tab navigation");
    })
    .expect("window");
    wait(cx, |_| mutations(&server) == 1);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn lost_permission_response_needs_exact_check_and_separate_activation(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("lost-runtime", "ses_parent");
    let server = request_server("ses_parent", true, false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    let mut request_key = None;
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            let request = ask(this, permission("ses_parent"), window, cx);
            this.reply_request(
                request.clone(),
                RequestReply::Permission(PermissionReply::Once),
                window,
                cx,
            );
            request_key = Some(request);
        })
    })
    .expect("window");
    let request_key = request_key.expect("key");
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace
                .read(cx)
                .blocking
                .stores
                .get(&request_key.scope)
                .and_then(|store| store.get(&request_key))
                .is_some_and(|entry| entry.response == ResponseState::Uncertain)
        })
    });
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.reply_request(
                request_key.clone(),
                RequestReply::Permission(PermissionReply::Once),
                window,
                cx,
            );
            this.check_request(request_key.clone(), window, cx);
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).request_actionable(&request_key))
    });
    assert_eq!(mutations(&server), 1, "lookup never repeats a decision");
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.reply_request(
                request_key.clone(),
                RequestReply::Permission(PermissionReply::Reject),
                window,
                cx,
            )
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| {
            workspace
                .read(cx)
                .blocking
                .stores
                .get(&request_key.scope)
                .expect("store")
                .get(&request_key)
                .is_none()
        })
    });
    assert_eq!(mutations(&server), 2);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn global_form_steps_acknowledge_only_visible_external_and_submit_typed_values(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new();
    let key = tests::key("form-runtime", "ses_parent");
    let server = request_server("ses_parent", false, false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            let request = ask(this, form(RequestOwner::Location), window, cx);
            assert!(
                !this.blocking.drafts[&request]
                    .answer(cx)
                    .contains_key("external")
            );
            this.blocking.drafts[&request].inputs["text"]
                .update(cx, |input, cx| input.set_value(" edited ", window, cx));
            this.set_form_step(request.clone(), 1, cx);
            assert!(
                !this.blocking.drafts[&request]
                    .answer(cx)
                    .contains_key("external")
            );
            this.set_form_step(request.clone(), 2, cx);
            assert_eq!(
                this.blocking.drafts[&request].answer(cx).get("external"),
                Some(&FormValue::Boolean(true))
            );
        });
        window.render_frame(cx);
        window.click(ElementId::Name("form-frm_dock-copy-json".into()), cx);
        let copied = cx
            .read_from_clipboard()
            .expect("clipboard")
            .text()
            .expect("text");
        let copied: Value = serde_json::from_str(&copied).expect("form export");
        assert!(copied.get("id").is_none() && copied.get("sessionID").is_none());
        assert_eq!(copied["fields"][0]["pattern"], ".*");
        window.click(ElementId::Name("form-frm_dock-copy-markdown".into()), cx);
        assert!(
            cx.read_from_clipboard()
                .expect("clipboard")
                .text()
                .expect("text")
                .contains("## external")
        );
        window.click(ElementId::Name("form-frm_dock-submit".into()), cx);
    })
    .expect("window");
    wait(cx, |_| mutations(&server) == 1);
    let requests = server.requests.lock().expect("requests");
    let request = requests
        .iter()
        .find(|request| request.path.contains("/form/") && request.method == "POST")
        .expect("answer");
    assert!(request.path.contains("/session/global/form/frm_dock/reply"));
    assert_eq!(
        serde_json::from_slice::<Value>(&request.body).expect("body"),
        json!({"answer":{"text":"edited","boolean":false,"external":true}})
    );
    drop(requests);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn superseding_send_preserves_prompt_on_lost_cancellation_and_never_posts(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("supersede-runtime", "ses_parent");
    let server = request_server("ses_parent", true, true);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            ask(this, permission("ses_parent"), window, cx);
            ask(this, form(RequestOwner::Location), window, cx);
            this.composer.update(cx, |input, cx| {
                input.set_value("superseding text", window, cx)
            });
            this.submit_connected(window, cx);
        })
    })
    .expect("window");
    wait(cx, |_| mutations(&server) == 1);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.composer
                .update(cx, |input, cx| input.set_value("newer text", window, cx))
        })
    })
    .expect("window");
    server.release_prompt.store(true, Ordering::Release);
    wait(cx, |cx| {
        cx.read(|cx| mutations(&server) == 2 && !workspace.read(cx).send_attempts.is_active(&key))
    });
    cx.read(|cx| {
        let this = workspace.read(cx);
        assert_eq!(
            this.composer.read(cx).value().as_ref(),
            "newer text\n\nsuperseding text"
        );
        assert_eq!(this.visible_requests().len(), 1);
    });
    assert_eq!(server.prompt_count(), 0);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn successful_superseding_send_retains_new_typing_and_posts_captured_text(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("supersede-success", "ses_parent");
    let server = request_server("ses_parent", false, true);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            ask(this, permission("ses_parent"), window, cx);
            this.composer
                .update(cx, |input, cx| input.set_value("captured text", window, cx));
            this.submit_connected(window, cx);
        })
    })
    .expect("window");
    wait(cx, |_| mutations(&server) == 1);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.composer
                .update(cx, |input, cx| input.set_value("new typing", window, cx))
        })
    })
    .expect("window");
    server.release_prompt.store(true, Ordering::Release);
    wait(cx, |cx| {
        cx.read(|cx| {
            server.prompt_count() == 1 && !workspace.read(cx).send_attempts.is_active(&key)
        })
    });
    cx.read(|cx| {
        assert_eq!(
            workspace.read(cx).composer.read(cx).value().as_ref(),
            "new typing"
        );
        assert!(workspace.read(cx).visible_requests().is_empty());
    });
    let requests = server.requests.lock().expect("requests");
    let prompt = requests
        .iter()
        .find(|request| request.path.ends_with("/prompt"))
        .expect("prompt");
    assert_eq!(
        serde_json::from_slice::<Value>(&prompt.body).expect("body")["text"],
        "captured text"
    );
    drop(requests);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn terminal_event_defeats_stale_list_and_drafts_are_runtime_scoped(cx: &mut TestAppContext) {
    use openchamber_native_core::pending_requests::RequestKind;
    use openchamber_native_core::request_list::RequestList;
    let fixture = Fixture::new();
    let key = tests::key("stale-runtime", "ses_parent");
    let server = request_server("ses_parent", false, false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            let request = form(RequestOwner::Location);
            let request_key = ask(this, request.clone(), window, cx);
            this.blocking.drafts[&request_key].inputs["text"].update(cx, |input, cx| {
                input.set_value("old runtime draft", window, cx)
            });
            this.switch_runtime(RuntimeId::from("new-request-runtime"), window, cx);
            let new_key = tests::key("new-request-runtime", "ses_parent");
            configure_send(this, &new_key, window, cx);
            let new_request_key = ask(this, request.clone(), window, cx);
            assert_eq!(
                this.blocking.drafts[&new_request_key]
                    .answer(cx)
                    .get("text"),
                Some(&FormValue::String("ready".into()))
            );
            assert_eq!(
                this.blocking.drafts[&request_key].answer(cx).get("text"),
                Some(&FormValue::String("old runtime draft".into()))
            );
            let load = this
                .blocking
                .stores
                .get_mut(&new_request_key.scope)
                .expect("store")
                .begin_load(RequestKind::Form);
            this.blocking_event(
                Some("/project".into()),
                BlockingRequestEvent::Settled {
                    owner: new_request_key.owner.clone(),
                    id: new_request_key.id.clone(),
                },
                window,
                cx,
            );
            let store = this
                .blocking
                .stores
                .get_mut(&new_request_key.scope)
                .expect("store");
            store
                .apply_list(
                    load,
                    RequestList::new(
                        new_request_key.scope.clone(),
                        None,
                        vec![request.clone()],
                        vec![],
                    ),
                )
                .expect("stale snapshot reconciles");
            this.blocking_event(
                Some("/project".into()),
                BlockingRequestEvent::Asked(request),
                window,
                cx,
            );
            assert!(this.visible_requests().is_empty());
            assert!(!this.blocking.drafts.contains_key(&new_request_key));
        })
    })
    .expect("window");
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn form_keyboard_submission_ignores_ime_preedit(cx: &mut TestAppContext) {
    use gpui_kit::EntityInputHandler;
    use gpui_kit::component::input::InputEvent;
    let fixture = Fixture::new();
    let key = tests::key("ime-runtime", "ses_parent");
    let server = request_server("ses_parent", false, false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    let mut input = None;
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            let PendingRequest::Form(mut form) = form(RequestOwner::Session(key.session.clone()))
            else {
                unreachable!()
            };
            form.fields.truncate(1);
            let request = ask(this, PendingRequest::Form(form), window, cx);
            let editor = this.blocking.drafts[&request].inputs["text"].clone();
            editor.update(cx, |input, cx| {
                input.focus(window, cx);
                input.replace_and_mark_text_in_range(None, "未", Some(0..1), window, cx);
                cx.emit(InputEvent::PressEnter {
                    secondary: true,
                    shift: false,
                });
            });
            input = Some(editor);
        })
    })
    .expect("window");
    cx.executor().run_until_parked();
    assert_eq!(
        mutations(&server),
        0,
        "IME candidate confirmation cannot submit"
    );
    cx.update_window(window, |_, window, cx| {
        input.as_ref().expect("editor").update(cx, |input, cx| {
            input.unmark_text(window, cx);
            cx.emit(InputEvent::PressEnter {
                secondary: true,
                shift: false,
            });
        });
    })
    .expect("window");
    wait(cx, |_| mutations(&server) == 1);
    close(cx, window, &workspace);
}

#[gpui_kit::test]
fn form_draft_types_conditions_and_switching_keep_exact_options(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let key = tests::key("types-runtime", "ses_parent");
    let server = request_server("ses_parent", false, false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            configure_send(this, &key, window, cx);
            let PendingRequest::Form(mut request) =
                form(RequestOwner::Session(key.session.clone()))
            else {
                unreachable!()
            };
            request.fields = vec![
                field(
                    "choice",
                    FormFieldKind::String {
                        format: None,
                        min_length: None,
                        max_length: None,
                        pattern: None,
                        placeholder: None,
                        default: Some(" ".into()),
                        options: Some(vec![FormOption {
                            value: " ".into(),
                            label: "Whitespace".into(),
                            description: None,
                        }]),
                        custom: true,
                    },
                ),
                field(
                    "number",
                    FormFieldKind::Number {
                        minimum: None,
                        maximum: None,
                        default: FormNumber::new(1.5),
                    },
                ),
                field(
                    "integer",
                    FormFieldKind::Integer {
                        minimum: None,
                        maximum: None,
                        default: FormNumber::new(3.),
                    },
                ),
                field(
                    "multi",
                    FormFieldKind::Multiselect {
                        options: vec![FormOption {
                            value: "a".into(),
                            label: "A".into(),
                            description: None,
                        }],
                        min_items: None,
                        max_items: None,
                        custom: true,
                        default: Some(vec!["a".into()]),
                    },
                ),
            ];
            request.fields[2]
                .when
                .push(openchamber_native_core::form::FormWhen {
                    key: "choice".into(),
                    op: openchamber_native_core::form::WhenOp::Equal,
                    value: FormValue::String(" ".into()),
                });
            let request = ask(this, PendingRequest::Form(request), window, cx);
            let draft = this.blocking.drafts.get_mut(&request).expect("draft");
            draft.custom.insert("multi".into());
            draft.inputs["multi"].update(cx, |input, cx| input.set_value(" extra ", window, cx));
            let answer = draft.answer(cx);
            assert_eq!(answer.get("choice"), Some(&FormValue::String(" ".into())));
            assert_eq!(
                answer.get("multi"),
                Some(&FormValue::Strings(vec!["a".into(), "extra".into()]))
            );
            assert!(answer.contains_key("integer"));
            draft.custom.insert("choice".into());
            draft.inputs["choice"].update(cx, |input, cx| input.set_value("different", window, cx));
            assert!(!draft.answer(cx).contains_key("integer"));
            this.show_new_session(window, cx);
            assert!(this.active_request().is_none());
            this.select_session(key.session.clone(), window, cx);
            assert_eq!(
                this.blocking.drafts[&request].answer(cx).get("multi"),
                Some(&FormValue::Strings(vec!["a".into(), "extra".into()]))
            );
        })
    })
    .expect("window");
    close(cx, window, &workspace);
}
