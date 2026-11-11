//! The Pinned messages section through the rendered window, the workspace's
//! completion handlers and a loopback server.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AnyWindowHandle, AppContext, ElementId, Entity, SharedString, Task, TestAppContext,
};
use openchamber_native_core::context_pins::{self, ContextPin, PinRole, PinTime};
use openchamber_native_core::ids::{MessageId, RuntimeId, SessionId};
use openchamber_native_core::session::{MetadataFacts, SessionPatch};
use openchamber_native_core::transcript::{Message, MessageInfo, Part, Role};
use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::event::ServerEvent;
use serde_json::{Value, json};

use super::pinned_section::PinChange;
use crate::connection::{Connection, ServerConfig};
use crate::locale;
use crate::settings_test_server::{self, Server};
use crate::workspace::{EarlierHistory, Workspace};
use crate::workspace_tests::open;

fn name(id: impl Into<String>) -> ElementId {
    ElementId::Name(SharedString::from(id.into()))
}

fn update<T>(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
    f: impl FnOnce(&mut Workspace, &mut gpui_kit::Window, &mut gpui_kit::Context<Workspace>) -> T,
) -> T {
    cx.update_window(window, |_, window, cx| {
        let result = workspace.update(cx, |workspace, cx| f(workspace, window, cx));
        window.render_frame(cx);
        result
    })
    .expect("window is open")
}

fn shown(window: AnyWindowHandle, id: impl Into<String>, cx: &mut TestAppContext) -> bool {
    let id = name(id);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .expect("window is open")
}

fn click(window: AnyWindowHandle, id: impl Into<String>, cx: &mut TestAppContext) {
    let id = name(id);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
        window.render_frame(cx);
    })
    .expect("window is open");
}

/// Selects the `index`th root session.
fn select_root(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    index: usize,
    cx: &mut TestAppContext,
) -> SessionId {
    update(window, workspace, cx, |this, window, cx| {
        let session = this
            .sessions
            .active()
            .iter()
            .filter(|record| record.parent_id.is_none() && record.directory.is_some())
            .nth(index)
            .expect("a root session")
            .id
            .clone();
        this.select_session(session.clone(), window, cx);
        session
    })
}

fn pin(id: &str, created_at: i64) -> ContextPin {
    ContextPin {
        id: MessageId::from(id),
        created_at: PinTime::from_millis(created_at),
        role: PinRole::User,
        session_id: None,
    }
}

/// Stores `pins` on `session` the way a server answer does.
fn store_pins(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    session: &SessionId,
    pins: Vec<ContextPin>,
    cx: &mut TestAppContext,
) {
    update(window, workspace, cx, |this, window, cx| {
        this.apply_server_events(
            vec![ServerEvent::SessionPatched {
                session_id: session.clone(),
                patch: SessionPatch {
                    metadata: Some(MetadataFacts {
                        context_pins: pins,
                        ..MetadataFacts::default()
                    }),
                    ..SessionPatch::default()
                },
            }],
            window,
            cx,
        );
    });
}

fn rows(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Vec<(String, Option<String>)> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .pinned_rows()
            .into_iter()
            .map(|row| {
                (
                    row.id.to_string(),
                    row.text.as_ref().map(ToString::to_string),
                )
            })
            .collect()
    })
}

fn notice(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Option<String> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .session_management
            .notice
            .as_ref()
            .map(ToString::to_string)
    })
}

fn string(key: &str, cx: &mut TestAppContext) -> String {
    cx.read(|cx| locale::strings(cx).get(key).to_string())
}

fn standing_serial(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Option<u64> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .message_focus
            .standing(Instant::now())
            .map(|request| request.serial)
    })
}

fn whole_message(id: &str, text: &str) -> Message {
    Message {
        info: MessageInfo {
            id: MessageId::from(id),
            role: Role::User,
            created: 5,
            completed: None,
            notice: None,
            assistant: None,
            compaction: None,
            synthetic: None,
        },
        parts: vec![Part::text(format!("{id}:text:0"), text)],
    }
}

fn user_envelope(id: &str, text: &str) -> Value {
    json!({ "id": id, "time": { "created": 5 }, "type": "user", "text": text })
}

/// Connects the workspace to `server` after its synthetic sessions opened.
fn connect(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    server: &Server,
    cx: &mut TestAppContext,
) {
    cx.update_window(window, |_, _, cx| {
        workspace.update(cx, |this, _| {
            let mut connection =
                Connection::new(ServerConfig::for_endpoint(server.endpoint.clone()))
                    .expect("network");
            connection.client = Some(
                ServerClient::new(server.endpoint.clone(), Credential::None)
                    .expect("fixture client"),
            );
            this.connection = Some(connection);
        });
    })
    .expect("window is open");
}

fn settle(window: AnyWindowHandle, cx: &mut TestAppContext) {
    // Each pass ends with a frame, which looks at what the last read left.
    for _ in 0..6 {
        settings_test_server::settle(window, cx);
    }
}

fn requests_to(server: &Server, path: &str) -> Vec<String> {
    server
        .requests()
        .into_iter()
        .filter(|request| request.is("GET", path))
        .map(|request| request.target)
        .collect()
}

#[gpui_kit::test]
fn pins_show_in_conversation_order_with_their_loaded_line_and_none_shows_nothing(
    cx: &mut TestAppContext,
) {
    let (window, workspace) = open(cx);
    let session = select_root(window, &workspace, 0, cx);
    assert!(
        !shown(window, "work-status-pinned", cx),
        "a session without pins has no section"
    );
    let (loaded, expected) = cx.read(|cx| {
        let this = workspace.read(cx);
        let message = this.open_sessions[&session]
            .transcript
            .messages()
            .iter()
            .find(|message| context_pins::first_text(message).is_some())
            .expect("the synthetic transcript has text");
        // A row keeps the first 512 characters of its line.
        let line: String = context_pins::first_text(message)
            .expect("found by its text")
            .chars()
            .take(512)
            .collect();
        (message.info.id.clone(), line)
    });
    store_pins(
        window,
        &workspace,
        &session,
        vec![
            pin("msg_late", 30),
            pin(loaded.as_str(), 20),
            pin("msg_b", 10),
            pin("msg_a", 10),
        ],
        cx,
    );
    assert!(shown(window, "work-status-pinned", cx));
    let listed = rows(&workspace, cx);
    assert_eq!(
        listed.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["msg_a", "msg_b", loaded.as_str(), "msg_late"]
    );
    assert_eq!(listed[2].1.as_deref(), Some(expected.as_str()));
    assert_eq!(
        listed[0].1, None,
        "a pin outside the loaded history shows the placeholder"
    );
    for (id, _) in &listed {
        assert!(shown(window, format!("work-status-pinned-reveal:{id}"), cx));
        assert!(shown(window, format!("work-status-pinned-unpin:{id}"), cx));
    }
    store_pins(window, &workspace, &session, Vec::new(), cx);
    assert!(!shown(window, "work-status-pinned", cx));
}

#[gpui_kit::test]
fn a_failed_text_read_keeps_its_row_and_is_not_read_again(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = select_root(window, &workspace, 0, cx);
    let old = format!("/api/session/{session}/message/msg_old");
    let broken = format!("/api/session/{session}/message/msg_broken");
    let (old_route, broken_route) = (old.clone(), broken.clone());
    let server = Server::new(move |request| {
        if request.is("GET", &old_route) {
            (
                200,
                json!({ "data": user_envelope("msg_old", "## Old **plan**\n> kept `here`") }),
            )
        } else if request.is("GET", &broken_route) {
            (500, json!({}))
        } else {
            (404, json!({}))
        }
    });
    connect(window, &workspace, &server, cx);
    store_pins(
        window,
        &workspace,
        &session,
        vec![pin("msg_old", 1), pin("msg_broken", 2)],
        cx,
    );
    settle(window, cx);
    assert_eq!(
        rows(&workspace, cx),
        [
            ("msg_old".to_owned(), Some("Old plan kept here".to_owned())),
            ("msg_broken".to_owned(), None),
        ],
        "one failed read leaves the other pin's line and its own row"
    );
    assert!(shown(window, "work-status-pinned-reveal:msg_broken", cx));
    // More frames and an unrelated change of the record start nothing.
    store_pins(
        window,
        &workspace,
        &session,
        vec![pin("msg_old", 1), pin("msg_broken", 2), pin("msg_third", 3)],
        cx,
    );
    settle(window, cx);
    assert_eq!(requests_to(&server, &old).len(), 1);
    assert_eq!(
        requests_to(&server, &broken).len(),
        1,
        "a failed read is remembered while the section stays on screen"
    );
    assert!(cx.read(|cx| workspace.read(cx).context_pins.reading().is_empty()));
    // Hiding the card drops what was fetched; showing it reads again.
    update(window, &workspace, cx, |this, _, _| {
        this.work_status_open = false
    });
    update(window, &workspace, cx, |this, _, _| {
        this.work_status_open = true
    });
    settle(window, cx);
    assert_eq!(requests_to(&server, &broken).len(), 2);
}

#[gpui_kit::test]
fn reads_of_a_left_session_directory_or_runtime_do_not_publish(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let first = select_root(window, &workspace, 0, cx);
    store_pins(window, &workspace, &first, vec![pin("msg_old", 1)], cx);
    let message = MessageId::from("msg_old");
    let finish = |epoch: u64,
                  window: AnyWindowHandle,
                  workspace: &Entity<Workspace>,
                  cx: &mut TestAppContext| {
        let message = message.clone();
        update(window, workspace, cx, move |this, _, cx| {
            this.finish_pinned_read(
                epoch,
                None,
                message,
                Ok(whole_message("msg_old", "late answer")),
                cx,
            );
        });
    };
    let epoch_of = |workspace: &Entity<Workspace>, cx: &mut TestAppContext| {
        cx.read(|cx| workspace.read(cx).context_pins.epoch())
    };

    // Another session.
    let retired = epoch_of(&workspace, cx);
    let second = select_root(window, &workspace, 1, cx);
    assert_ne!(first, second);
    store_pins(window, &workspace, &second, vec![pin("msg_old", 1)], cx);
    finish(retired, window, &workspace, cx);
    assert_eq!(
        rows(&workspace, cx),
        [("msg_old".to_owned(), None)],
        "the other session's read does not fill this session's row"
    );

    // A read of the current scope does publish.
    finish(epoch_of(&workspace, cx), window, &workspace, cx);
    assert_eq!(
        rows(&workspace, cx),
        [("msg_old".to_owned(), Some("late answer".to_owned()))]
    );

    // The same session under another directory: what was fetched is dropped
    // and a read of the old directory is not kept.
    let retired = epoch_of(&workspace, cx);
    update(window, &workspace, cx, |this, window, cx| {
        this.apply_server_events(
            vec![ServerEvent::SessionPatched {
                session_id: second.clone(),
                patch: SessionPatch {
                    directory: Some("/moved/elsewhere".to_owned()),
                    ..SessionPatch::default()
                },
            }],
            window,
            cx,
        );
    });
    assert_ne!(retired, epoch_of(&workspace, cx));
    finish(retired, window, &workspace, cx);
    assert!(
        rows(&workspace, cx).iter().all(|(_, text)| text.is_none()),
        "no line of the old directory shows"
    );

    // Another runtime.
    let retired = epoch_of(&workspace, cx);
    update(window, &workspace, cx, |this, window, cx| {
        this.switch_runtime(RuntimeId::from("another-runtime"), window, cx);
    });
    assert_ne!(retired, epoch_of(&workspace, cx));
    finish(retired, window, &workspace, cx);
    assert!(
        !cx.read(|cx| workspace.read(cx).context_pins.fetched(&message)),
        "a read of the old runtime is not kept for the new one"
    );
}

#[gpui_kit::test]
fn a_failed_unpin_keeps_the_row_and_only_its_button_is_held(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = select_root(window, &workspace, 0, cx);
    store_pins(
        window,
        &workspace,
        &session,
        vec![pin("msg_a", 1), pin("msg_b", 2)],
        cx,
    );
    let failed = string("chat.workStatus.pinned.unpinFailed", cx);
    assert_eq!(failed, "Could not unpin the message");

    // Without a server the unpin cannot start: the row stays and says so.
    click(window, "work-status-pinned-unpin:msg_a", cx);
    assert_eq!(notice(&workspace, cx).as_deref(), Some(failed.as_str()));
    assert_eq!(rows(&workspace, cx).len(), 2);
    assert_eq!(
        standing_serial(&workspace, cx),
        None,
        "the pin's press is not the row's"
    );

    // While one unpin runs, its own button and the other pins' do nothing.
    let key = update(window, &workspace, cx, |this, _, _| {
        this.session_management.notice = None;
        let key = this.mutation_key(&session);
        this.context_pins
            .begin_unpin(key.clone(), MessageId::from("msg_a"));
        key
    });
    click(window, "work-status-pinned-unpin:msg_a", cx);
    click(window, "work-status-pinned-unpin:msg_b", cx);
    assert_eq!(
        cx.read(|cx| workspace.read(cx).context_pins.unpinning().cloned()),
        Some(MessageId::from("msg_a"))
    );
    assert_eq!(notice(&workspace, cx), None, "no second unpin started");
    // The other row still goes to its message.
    click(window, "work-status-pinned-reveal:msg_b", cx);
    assert_eq!(
        notice(&workspace, cx).as_deref(),
        Some(string("chat.messageLink.notFound", cx).as_str()),
        "the synthetic session has no older history to search"
    );

    // The server refuses: the row stays, the failure is said, the pin frees.
    update(window, &workspace, cx, |this, window, cx| {
        this.finish_context_pin_change(
            key.clone(),
            PinChange::unpin(MessageId::from("msg_a")),
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        );
    });
    assert_eq!(notice(&workspace, cx).as_deref(), Some(failed.as_str()));
    assert_eq!(rows(&workspace, cx).len(), 2);
    assert!(shown(window, "work-status-pinned-unpin:msg_a", cx));
    assert_eq!(
        cx.read(|cx| workspace.read(cx).context_pins.unpinning().cloned()),
        None
    );

    // A stale completion of another unpin does not free a running one, and
    // one for another runtime says nothing.
    update(window, &workspace, cx, |this, window, cx| {
        this.session_management.notice = None;
        this.context_pins
            .begin_unpin(key.clone(), MessageId::from("msg_b"));
        let mut other = key.clone();
        other.runtime = RuntimeId::from("another-runtime");
        this.finish_context_pin_change(
            other,
            PinChange::unpin(MessageId::from("msg_b")),
            Some(Err(TransportError::Http { status: 500 })),
            window,
            cx,
        );
        assert!(this.session_management.notice.is_none());
        assert!(this.context_pins.unpinning().is_some());
        // The server's answer is what removes the row.
        this.finish_context_pin_change(
            key.clone(),
            PinChange::unpin(MessageId::from("msg_b")),
            Some(Ok(MetadataFacts {
                context_pins: vec![pin("msg_a", 1)],
                ..MetadataFacts::default()
            })),
            window,
            cx,
        );
    });
    assert_eq!(rows(&workspace, cx), [("msg_a".to_owned(), None)]);
    assert!(!shown(window, "work-status-pinned-unpin:msg_b", cx));
}

#[gpui_kit::test]
fn every_press_on_a_row_is_a_new_request_and_another_session_ends_it(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = select_root(window, &workspace, 0, cx);
    store_pins(window, &workspace, &session, vec![pin("msg_old", 1)], cx);
    // The transcript's own loader is already reading an older page.
    update(window, &workspace, cx, |this, _, _| {
        let open = this.open_sessions.get_mut(&session).expect("visited");
        open.older = Some("cursor-1".to_owned());
        open.earlier = EarlierHistory::Loading {
            _read: Task::ready(()),
        };
    });
    click(window, "work-status-pinned-reveal:msg_old", cx);
    let first = standing_serial(&workspace, cx).expect("the request stands while the page loads");
    click(window, "work-status-pinned-reveal:msg_old", cx);
    let second = standing_serial(&workspace, cx).expect("the repeated press stands too");
    assert_ne!(first, second, "a repeated press is a new request");
    let in_flight = cx.read(|cx| {
        workspace
            .read(cx)
            .message_focus
            .in_flight(&session, Instant::now())
            .cloned()
    });
    assert_eq!(in_flight, Some(MessageId::from("msg_old")));
    assert_eq!(notice(&workspace, cx), None);

    // The page the loader was reading lands with the message.
    update(window, &workspace, cx, |this, _, _| {
        let open = this.open_sessions.get_mut(&session).expect("visited");
        let _ = open
            .transcript
            .push(whole_message("msg_old", "an old prompt"));
        open.list.reset(open.transcript.len());
        open.older = None;
        open.earlier = EarlierHistory::Idle;
    });
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .expect("window is open");
    assert_eq!(
        standing_serial(&workspace, cx),
        None,
        "the request ends when its message is shown"
    );
    assert_eq!(notice(&workspace, cx), None);
    assert_eq!(
        rows(&workspace, cx),
        [("msg_old".to_owned(), Some("an old prompt".to_owned()))]
    );

    // A request for a message still loading leaves with its session.
    update(window, &workspace, cx, |this, window, cx| {
        let open = this.open_sessions.get_mut(&session).expect("visited");
        open.older = Some("cursor-2".to_owned());
        open.earlier = EarlierHistory::Loading {
            _read: Task::ready(()),
        };
        this.request_message_focus(&session, &MessageId::from("msg_older"), window, cx);
    });
    assert!(standing_serial(&workspace, cx).is_some());
    let other = select_root(window, &workspace, 1, cx);
    assert_ne!(other, session);
    assert_eq!(standing_serial(&workspace, cx), None);
}

#[gpui_kit::test]
fn an_old_message_is_reached_through_the_history_loader(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = select_root(window, &workspace, 0, cx);
    let whole = format!("/api/session/{session}/message/msg_old");
    let pages = format!("/api/session/{session}/message");
    let (whole_route, pages_route) = (whole.clone(), pages.clone());
    let server = Server::new(move |request| {
        if request.is("GET", &whole_route) {
            return (
                200,
                json!({ "data": user_envelope("msg_old", "the old prompt") }),
            );
        }
        if !request.is("GET", &pages_route) {
            return (404, json!({}));
        }
        if request.target.contains("cursor=cursor-1") {
            // A full page that does not hold the message.
            let page: Vec<Value> = (0..100)
                .map(|index| user_envelope(&format!("msg_page_{index:03}"), "filler"))
                .collect();
            (
                200,
                json!({ "data": page, "cursor": { "next": "cursor-2" } }),
            )
        } else if request.target.contains("cursor=cursor-2") {
            (
                200,
                json!({ "data": [user_envelope("msg_old", "the old prompt")], "cursor": {} }),
            )
        } else {
            (404, json!({}))
        }
    });
    connect(window, &workspace, &server, cx);
    store_pins(window, &workspace, &session, vec![pin("msg_old", 1)], cx);
    update(window, &workspace, cx, |this, _, _| {
        this.open_sessions.get_mut(&session).expect("visited").older = Some("cursor-1".to_owned());
    });
    click(window, "work-status-pinned-reveal:msg_old", cx);
    assert!(
        standing_serial(&workspace, cx).is_some(),
        "the row spins while the search runs"
    );
    settle(window, cx);
    let cursors: Vec<String> = requests_to(&server, &pages)
        .into_iter()
        .filter(|target| target.contains("cursor="))
        .collect();
    assert_eq!(cursors.len(), 2, "one read per page: {cursors:?}");
    assert!(cursors[0].contains("cursor=cursor-1"));
    assert!(cursors[1].contains("cursor=cursor-2"));
    cx.read(|cx| {
        let this = workspace.read(cx);
        let open = &this.open_sessions[&session];
        assert!(
            open.transcript
                .row_of(&MessageId::from("msg_old"))
                .is_some()
        );
        assert!(matches!(open.earlier, EarlierHistory::Idle));
        assert_eq!(open.older, None);
    });
    assert_eq!(standing_serial(&workspace, cx), None);
    assert_eq!(notice(&workspace, cx), None);
}

#[gpui_kit::test]
fn a_message_the_server_does_not_know_is_ruled_out_before_any_page(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = select_root(window, &workspace, 0, cx);
    let pages = format!("/api/session/{session}/message");
    let server = Server::new(|_| (404, json!({})));
    connect(window, &workspace, &server, cx);
    store_pins(window, &workspace, &session, vec![pin("msg_gone", 1)], cx);
    update(window, &workspace, cx, |this, _, _| {
        this.open_sessions.get_mut(&session).expect("visited").older = Some("cursor-1".to_owned());
    });
    settle(window, cx);
    click(window, "work-status-pinned-reveal:msg_gone", cx);
    settle(window, cx);
    assert_eq!(
        notice(&workspace, cx).as_deref(),
        Some(string("chat.messageLink.notFound", cx).as_str())
    );
    assert_eq!(standing_serial(&workspace, cx), None);
    assert!(
        requests_to(&server, &pages).is_empty(),
        "no history page is read for a message that does not exist"
    );
    assert_eq!(
        rows(&workspace, cx),
        [("msg_gone".to_owned(), None)],
        "the pin's row stays"
    );
}

#[gpui_kit::test]
fn the_footer_pin_change_frees_its_button_and_reports_a_refusal(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = select_root(window, &workspace, 0, cx);
    let message = cx.read(|cx| {
        workspace.read(cx).open_sessions[&session]
            .transcript
            .messages()
            .iter()
            .find(|message| message.info.role == Role::Assistant && message.info.created != 0)
            .expect("the synthetic transcript has an answer")
            .info
            .clone()
    });
    // Without a server the change cannot start and says so.
    update(window, &workspace, cx, |this, window, cx| {
        this.toggle_context_pin(&session, &message.id, window, cx);
    });
    assert_eq!(
        notice(&workspace, cx).as_deref(),
        Some(string("chat.messageBody.actions.contextPinFailed", cx).as_str())
    );
    assert!(rows(&workspace, cx).is_empty());
    // The server's answer pins the message for this session.
    update(window, &workspace, cx, |this, window, cx| {
        let key = this.mutation_key(&session);
        this.finish_context_pin_change(
            key,
            PinChange::toggle(message.id.clone(), None),
            Some(Ok(MetadataFacts {
                context_pins: vec![ContextPin {
                    id: message.id.clone(),
                    created_at: PinTime::from_millis(message.created),
                    role: PinRole::Assistant,
                    session_id: Some(session.clone()),
                }],
                ..MetadataFacts::default()
            })),
            window,
            cx,
        );
    });
    assert_eq!(rows(&workspace, cx).len(), 1);
    assert!(cx.read(|cx| {
        let this = workspace.read(cx);
        context_pins::is_pinned(this.session(&session).expect("listed"), &message.id)
    }));
}

/// Replaces the connection's client, as a finished reconnect does.
fn restore_client(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    server: &Server,
    cx: &mut TestAppContext,
) {
    update(window, workspace, cx, |this, _, _| {
        this.connection.as_mut().expect("connected").client = Some(
            ServerClient::new(server.endpoint.clone(), Credential::None).expect("fixture client"),
        );
    });
}

#[gpui_kit::test]
fn a_reconnect_drops_the_running_read_and_the_line_is_read_again(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = select_root(window, &workspace, 0, cx);
    let old = format!("/api/session/{session}/message/msg_old");
    let route = old.clone();
    let server = Server::new(move |request| {
        if request.is("GET", &route) {
            (
                200,
                json!({ "data": user_envelope("msg_old", "## Old **plan**") }),
            )
        } else {
            (404, json!({}))
        }
    });
    connect(window, &workspace, &server, cx);
    // The frame after this starts the read; the connection restarts before
    // its answer is looked at.
    store_pins(window, &workspace, &session, vec![pin("msg_old", 1)], cx);
    let message = MessageId::from("msg_old");
    assert_eq!(
        cx.read(|cx| workspace.read(cx).context_pins.reading()),
        std::slice::from_ref(&message)
    );
    update(window, &workspace, cx, |this, _, _| {
        this.connection.as_mut().expect("connected").restart();
    });
    settle(window, cx);
    cx.read(|cx| {
        let pins = &workspace.read(cx).context_pins;
        assert!(
            !pins.fetched(&message),
            "neither the old connection's answer nor the missing client is recorded"
        );
        assert!(
            pins.reading().is_empty(),
            "nothing is read without a client"
        );
    });
    assert_eq!(rows(&workspace, cx), [("msg_old".to_owned(), None)]);
    let before = requests_to(&server, &old).len();
    restore_client(window, &workspace, &server, cx);
    settle(window, cx);
    assert_eq!(
        rows(&workspace, cx),
        [("msg_old".to_owned(), Some("Old plan".to_owned()))],
        "the line is read once the client is back"
    );
    assert_eq!(requests_to(&server, &old).len(), before + 1);

    // An answer that arrives for an older connection generation is dropped
    // even when it is a failure, so it cannot become a lasting placeholder.
    update(window, &workspace, cx, |this, _, cx| {
        this.work_status_open = false;
        cx.notify();
    });
    update(window, &workspace, cx, |this, _, cx| {
        this.connection.as_mut().expect("connected").client = None;
        this.work_status_open = true;
        cx.notify();
    });
    update(window, &workspace, cx, |this, _, cx| {
        let epoch = this.context_pins.epoch();
        let stale = this
            .connection
            .as_ref()
            .map(|connection| connection.generation - 1);
        this.finish_pinned_read(
            epoch,
            stale,
            MessageId::from("msg_old"),
            Err(TransportError::Http { status: 500 }),
            cx,
        );
        assert!(!this.context_pins.fetched(&MessageId::from("msg_old")));
    });
}

#[gpui_kit::test]
fn pin_changes_of_one_session_run_one_after_another(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = select_root(window, &workspace, 0, cx);
    let answer = cx.read(|cx| {
        workspace.read(cx).open_sessions[&session]
            .transcript
            .messages()
            .iter()
            .find(|message| message.info.role == Role::Assistant && message.info.created != 0)
            .expect("the synthetic transcript has an answer")
            .info
            .id
            .clone()
    });
    let entry = |id: &str, created_at: i64| json!({ "id": id, "createdAt": created_at, "role": "user", "sessionID": session.as_str() });
    // The server keeps the list and answers every write with what it holds.
    let stored = Arc::new(Mutex::new(json!([entry("msg_a", 1), entry("msg_b", 2)])));
    let record = format!("/api/session/{session}");
    let metadata = format!("/api/openchamber/sessions/{session}/metadata");
    let (held, record_route, metadata_route, id) = (
        Arc::clone(&stored),
        record.clone(),
        metadata.clone(),
        session.to_string(),
    );
    let server = Server::new(move |request| {
        let mut held = held.lock().expect("stored pins");
        if request.is("GET", &record_route) {
            return (
                200,
                json!({ "data": {
                    "id": id,
                    "location": { "directory": "/p" },
                    "time": { "created": 1, "updated": 2 },
                    "metadata": { "openchamber": { "context_obligatory_messages": *held } },
                } }),
            );
        }
        if request.is("POST", &metadata_route) {
            *held = request.body["patch"]["openchamber"]["context_obligatory_messages"].clone();
            return (
                200,
                json!({ "metadata": { "openchamber": { "context_obligatory_messages": *held } } }),
            );
        }
        (404, json!({}))
    });
    connect(window, &workspace, &server, cx);
    store_pins(
        window,
        &workspace,
        &session,
        vec![pin("msg_a", 1), pin("msg_b", 2)],
        cx,
    );
    // Pin the answer, and unpin B before the pin was answered.
    update(window, &workspace, cx, |this, window, cx| {
        this.toggle_context_pin(&session, &answer, window, cx);
    });
    click(window, "work-status-pinned-unpin:msg_b", cx);
    assert_eq!(
        cx.read(|cx| workspace.read(cx).context_pins.unpinning().cloned()),
        Some(MessageId::from("msg_b")),
        "the unpin waits behind the pin and holds its button"
    );
    settle(window, cx);
    let ids = |list: &Value| -> Vec<String> {
        list.as_array()
            .expect("a list")
            .iter()
            .map(|entry| entry["id"].as_str().expect("an id").to_owned())
            .collect()
    };
    let writes: Vec<Vec<String>> = server
        .writes()
        .iter()
        .filter(|request| request.is("POST", &metadata))
        .map(|request| ids(&request.body["patch"]["openchamber"]["context_obligatory_messages"]))
        .collect();
    assert_eq!(
        writes,
        [
            vec!["msg_a".to_owned(), "msg_b".to_owned(), answer.to_string()],
            vec!["msg_a".to_owned(), answer.to_string()],
        ],
        "the unpin starts from a read that already holds the pin"
    );
    assert_eq!(
        ids(&stored.lock().expect("stored pins")),
        ["msg_a".to_owned(), answer.to_string()]
    );
    assert_eq!(
        rows(&workspace, cx)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        ["msg_a".to_owned(), answer.to_string()]
    );
    assert_eq!(notice(&workspace, cx), None);
    assert_eq!(
        cx.read(|cx| workspace.read(cx).context_pins.unpinning().cloned()),
        None
    );
}

#[gpui_kit::test]
fn leaving_every_session_ends_the_request_and_a_press_leaves_the_overlay(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = select_root(window, &workspace, 0, cx);
    store_pins(window, &workspace, &session, vec![pin("msg_old", 1)], cx);
    let loading = |this: &mut Workspace| {
        let open = this.open_sessions.get_mut(&session).expect("visited");
        open.older = Some("cursor-1".to_owned());
        open.earlier = EarlierHistory::Loading {
            _read: Task::ready(()),
        };
    };
    // The card lies over the transcript: going to a message leaves it. No
    // frame runs in between, because a window this wide shows the card
    // beside the chat and clears the overlay itself.
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            loading(this);
            this.work_status_overlay = true;
            this.reveal_pinned_message(&MessageId::from("msg_old"), window, cx);
            assert!(!this.work_status_overlay);
            assert!(this.message_focus.standing(Instant::now()).is_some());
        });
    })
    .expect("window is open");
    // The new-session view enters no session, so the request ends there.
    update(window, &workspace, cx, |this, window, cx| {
        this.show_new_session(window, cx);
    });
    assert_eq!(standing_serial(&workspace, cx), None);
    assert_eq!(notice(&workspace, cx), None);
    // The same holds for closing the session without navigating.
    let session = select_root(window, &workspace, 0, cx);
    update(window, &workspace, cx, |this, window, cx| {
        this.request_message_focus(&session, &MessageId::from("msg_older"), window, cx);
        assert!(this.message_focus.standing(Instant::now()).is_some());
        this.close_selected_session(window, cx);
        assert!(this.message_focus.standing(Instant::now()).is_none());
    });
}

#[gpui_kit::test]
fn a_request_waits_for_the_client_and_does_not_report_a_missing_message(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    let session = select_root(window, &workspace, 0, cx);
    let whole = format!("/api/session/{session}/message/msg_old");
    let pages = format!("/api/session/{session}/message");
    let (whole_route, pages_route) = (whole.clone(), pages.clone());
    let server = Server::new(move |request| {
        if request.is("GET", &whole_route) {
            (
                200,
                json!({ "data": user_envelope("msg_old", "the old prompt") }),
            )
        } else if request.is("GET", &pages_route) && request.target.contains("cursor=cursor-1") {
            (
                200,
                json!({ "data": [user_envelope("msg_old", "the old prompt")], "cursor": {} }),
            )
        } else {
            (404, json!({}))
        }
    });
    connect(window, &workspace, &server, cx);
    update(window, &workspace, cx, |this, _, _| {
        this.connection.as_mut().expect("connected").client = None;
        this.open_sessions.get_mut(&session).expect("visited").older = Some("cursor-1".to_owned());
    });
    store_pins(window, &workspace, &session, vec![pin("msg_old", 1)], cx);
    click(window, "work-status-pinned-reveal:msg_old", cx);
    settle(window, cx);
    assert!(
        standing_serial(&workspace, cx).is_some(),
        "the request stands while the connection has no client"
    );
    assert_eq!(
        notice(&workspace, cx),
        None,
        "nothing says the message is missing"
    );
    assert!(requests_to(&server, &whole).is_empty());
    assert!(requests_to(&server, &pages).is_empty());
    restore_client(window, &workspace, &server, cx);
    settle(window, cx);
    assert_eq!(standing_serial(&workspace, cx), None);
    assert_eq!(notice(&workspace, cx), None);
    assert!(!requests_to(&server, &whole).is_empty());
    assert!(cx.read(|cx| {
        workspace.read(cx).open_sessions[&session]
            .transcript
            .row_of(&MessageId::from("msg_old"))
            .is_some()
    }));
}
