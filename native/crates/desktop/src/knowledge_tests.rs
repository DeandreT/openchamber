//! The Project knowledge panel against a scripted loopback server that
//! answers the project-context routes and records every request.

use std::sync::{Arc, Mutex};

use gpui_kit::{AnyWindowHandle, AppContext, Entity, EntityInputHandler, TestAppContext};
use openchamber_native_core::knowledge::Section;
use serde_json::{Value, json};

use crate::files_view::PanelScope;
use crate::knowledge::{KnowledgeEvent, KnowledgeOwner, KnowledgePanel, PlanLoad};
use crate::network::Network;
use crate::scheduled_tasks_tests::{Scripted, wait};

#[path = "knowledge_lifecycle_tests.rs"]
mod lifecycle;

const ROOT: &str = "/work/project";

fn base() -> String {
    let id = openchamber_native_core::ids::ProjectId::from_path(ROOT).expect("an id");
    format!("/api/project-context/{}", id.as_str())
}

#[derive(Default)]
struct Store {
    fail_writes: bool,
    todos: Vec<Value>,
    notes: Vec<Value>,
}

impl Store {
    fn context(&self) -> Value {
        json!({
            "notes": self.notes,
            "todos": self.todos,
            "plans": [{"id": "p1", "title": "Launch", "createdAt": 1, "progress": {"done": 1, "total": 3}}],
            "sharedPlansDir": null
        })
    }

    fn answer(&mut self, method: &str, path: &str, body: &Value) -> (u16, Value) {
        let base = base();
        let rest = path.strip_prefix(&base).unwrap_or("?");
        if method != "GET" && self.fail_writes {
            return (500, json!({"error": "disk full"}));
        }
        match (method, rest) {
            ("GET", "") => (200, self.context()),
            ("POST", "/todos") => {
                self.todos.push(json!({"id": body["id"], "text": body["text"], "completed": false, "createdAt": 1}));
                (201, self.context())
            }
            ("PATCH", todo) if todo.starts_with("/todos/") => {
                let id = &todo["/todos/".len()..];
                for stored in &mut self.todos {
                    if stored["id"] == json!(id) {
                        for key in ["completed", "text"] {
                            if let Some(value) = body.get(key) {
                                stored[key] = value.clone();
                            }
                        }
                    }
                }
                (200, self.context())
            }
            ("PATCH", "/notes/n1") => {
                self.notes[0]["body"] = body["body"].clone();
                (200, json!({"note": self.notes[0]}))
            }
            ("DELETE", "/notes/n1") => {
                self.notes.clear();
                (200, self.context())
            }
            ("GET", "/plans/p1") => (
                200,
                json!({"id": "p1", "title": "Launch", "createdAt": 1, "raw": "# Launch\n\n## Steps\n- [ ] one\n", "body": "", "path": "/plans/launch.md"}),
            ),
            ("PUT", "/plans/p1") => (
                200,
                json!({"plan": {"id": "p1", "title": "Launch", "createdAt": 1}, "raw": body["raw"]}),
            ),
            _ => (404, json!({"error": "unexpected route"})),
        }
    }
}

fn serve(store: Store) -> (Scripted, Arc<Mutex<Store>>) {
    let store = Arc::new(Mutex::new(store));
    let shared = store.clone();
    let server = Scripted::new(move |request| {
        shared
            .lock()
            .expect("store")
            .answer(&request.method, &request.path, &request.body)
    });
    (server, store)
}

/// The panel in the workspace's window, its events recorded.
fn open_panel(
    cx: &mut TestAppContext,
    server: &Scripted,
    network: &Network,
) -> (
    AnyWindowHandle,
    Entity<KnowledgePanel>,
    Arc<Mutex<Vec<KnowledgeEvent>>>,
) {
    cx.executor().allow_parking();
    let (window, workspace) = crate::workspace_tests::open(cx);
    let client = server.client();
    let handle = network.handle();
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let panel = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let scope = PanelScope {
                    client,
                    network: handle,
                    runtime: workspace.runtime.clone(),
                    directory: ROOT.to_owned(),
                    owner: None,
                };
                let owner = KnowledgeOwner::new(ROOT, "project".into()).expect("an owner");
                let panel = cx.new(|cx| KnowledgePanel::new(scope, owner, window, cx));
                cx.subscribe(&panel, move |_, _, event: &KnowledgeEvent, _| {
                    log.lock().expect("events").push(event.clone());
                })
                .detach();
                panel
            })
        })
        .expect("window is open");
    (window, panel, events)
}

#[gpui_kit::test]
fn todo_edits_commit_once_preserve_metadata_and_revert_failed_writes(cx: &mut TestAppContext) {
    let original = json!({"id": "t1", "text": "Original", "completed": true,
        "createdAt": 7, "planId": "p1", "dependsOn": ["t0"]});
    let (server, store) = serve(Store {
        todos: vec![original.clone()],
        ..Store::default()
    });
    let network = Network::new().expect("network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit_todo("t1", window, cx);
            let input = panel.todo_edit.as_ref().expect("editing").input.clone();
            input.update(cx, |input, cx| {
                input.set_value("  Revised\r\nשלום  ", window, cx)
            });
            panel.finish_todo_edit(&input, true, window, cx);
            panel.finish_todo_edit(&input, true, window, cx);
            let todo = &panel.knowledge.as_ref().expect("loaded").todos[0];
            assert_eq!(todo.text, "Revised שלום");
            assert!(todo.completed);
            assert_eq!(todo.created_at, 7);
            assert_eq!(todo.plan_id.as_deref(), Some("p1"));
            assert_eq!(todo.depends_on, ["t0"]);
        });
    })
    .expect("open window");
    wait(cx, |cx| cx.read(|cx| panel.read(cx).todo_writes == 0));
    assert_eq!(
        server
            .requests("PATCH", &format!("{}/todos/t1", base()))
            .len(),
        1
    );
    assert_eq!(
        store.lock().expect("store").todos[0]["text"],
        "Revised שלום"
    );

    store.lock().expect("store").fail_writes = true;
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit_todo("t1", window, cx);
            let input = panel.todo_edit.as_ref().expect("editing").input.clone();
            input.update(cx, |input, cx| input.set_value("Rejected edit", window, cx));
            panel.finish_todo_edit(&input, true, window, cx);
            assert_eq!(
                panel.knowledge.as_ref().expect("loaded").todos[0].text,
                "Rejected edit"
            );
        });
    })
    .expect("open window");
    wait(cx, |cx| {
        cx.read(|cx| {
            let panel = panel.read(cx);
            panel.todo_writes == 0
                && !panel.loading
                && panel
                    .knowledge
                    .as_ref()
                    .is_some_and(|k| k.todos[0].text == "Revised שלום")
        })
    });
    assert!(cx.read(|cx| {
        panel
            .read(cx)
            .toast
            .as_ref()
            .is_some_and(|toast| toast.error)
    }));
}

#[gpui_kit::test]
fn todo_editor_blur_commits_the_normalized_input(cx: &mut TestAppContext) {
    let (server, store) = serve(Store {
        todos: vec![json!({"id": "t1", "text": "Original", "createdAt": 1})],
        ..Store::default()
    });
    let network = Network::new().expect("network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    let input = cx
        .update_window(window, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.edit_todo("t1", window, cx);
                let input = panel.todo_edit.as_ref().expect("editing").input.clone();
                input.update(cx, |input, cx| {
                    input.replace_text_in_range(Some(0..8), "  First\nsecond  ", window, cx);
                });
                input
            })
        })
        .expect("open window");
    cx.run_until_parked();
    assert_eq!(
        cx.read(|cx| input.read(cx).value().to_string()),
        "  First second  "
    );
    cx.update(|cx| {
        input.update(cx, |_, cx| {
            cx.emit(gpui_kit::component::input::InputEvent::Blur);
        })
    });
    wait(cx, |cx| {
        cx.read(|cx| panel.read(cx).todo_edit.is_none() && panel.read(cx).todo_writes == 0)
    });
    assert_eq!(
        store.lock().expect("store").todos[0]["text"],
        "First second"
    );
    assert_eq!(
        server
            .requests("PATCH", &format!("{}/todos/t1", base()))
            .len(),
        1
    );
}

#[gpui_kit::test]
fn todo_edit_cancellation_composition_and_stale_inputs_make_no_writes(cx: &mut TestAppContext) {
    let (server, _) = serve(Store {
        todos: vec![json!({"id": "t1", "text": "Original", "createdAt": 1})],
        ..Store::default()
    });
    let network = Network::new().expect("network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            for (value, commit) in [("Changed", false), ("  ", true), ("Original", true)] {
                panel.edit_todo("t1", window, cx);
                let input = panel.todo_edit.as_ref().expect("editing").input.clone();
                input.update(cx, |input, cx| input.set_value(value, window, cx));
                panel.finish_todo_edit(&input, commit, window, cx);
                assert!(panel.todo_edit.is_none());
            }
            panel.edit_todo("t1", window, cx);
            let old = panel.todo_edit.as_ref().expect("editing").input.clone();
            old.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "仮", Some(0..1), window, cx)
            });
            panel.finish_todo_edit(&old, true, window, cx);
            panel.finish_todo_edit(&old, false, window, cx);
            assert!(panel.todo_edit.is_some(), "IME keys cannot finish the edit");
            old.update(cx, |input, cx| input.unmark_text(window, cx));
            panel.finish_todo_edit(&old, false, window, cx);
            panel.edit_todo("t1", window, cx);
            let current = panel.todo_edit.as_ref().expect("new edit").input.clone();
            panel.finish_todo_edit(&old, true, window, cx);
            assert_eq!(
                panel.todo_edit.as_ref().expect("new edit stays").input,
                current
            );
            panel.knowledge.as_mut().expect("loaded").todos.clear();
            current.update(cx, |input, cx| {
                input.set_value("Cannot recreate", window, cx)
            });
            panel.finish_todo_edit(&current, true, window, cx);
            assert!(panel.todo_edit.is_none());
        });
    })
    .expect("open window");
    cx.run_until_parked();
    assert!(
        server
            .requests("PATCH", &format!("{}/todos/t1", base()))
            .is_empty()
    );
}

#[gpui_kit::test]
fn todos_show_at_once_and_a_failed_write_reads_the_list_again(cx: &mut TestAppContext) {
    let (server, store) = serve(Store::default());
    let network = Network::new().expect("a network runtime");
    let (window, panel, events) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    assert_eq!(cx.read(|cx| panel.read(cx).counts().plans), 1);

    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.select_section(Section::Todos, cx);
            panel.todo_input.update(cx, |input, cx| {
                input.set_value("Write release notes", window, cx)
            });
            panel.add_todo(window, cx);
        })
    })
    .expect("window is open");
    // Shown before the server answers.
    assert_eq!(
        cx.read(|cx| panel.read(cx).knowledge.as_ref().map(|k| k.todos.len())),
        Some(1)
    );
    wait(cx, |_| {
        !server
            .requests("POST", &format!("{}/todos", base()))
            .is_empty()
    });
    let created = &server.requests("POST", &format!("{}/todos", base()))[0].body;
    assert_eq!(created["text"], json!("Write release notes"));
    let id = created["id"].as_str().expect("an id").to_owned();
    wait(cx, |cx| cx.read(|cx| panel.read(cx).todo_writes == 0));

    store.lock().expect("store").fail_writes = true;
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| panel.toggle_todo(&id, window, cx))
    })
    .expect("window is open");
    assert!(cx.read(|cx| {
        panel
            .read(cx)
            .knowledge
            .as_ref()
            .is_some_and(|k| k.todos[0].completed)
    }));
    wait(cx, |cx| {
        cx.read(|cx| {
            let panel = panel.read(cx);
            panel.todo_writes == 0
                && !panel.loading
                && panel
                    .knowledge
                    .as_ref()
                    .is_some_and(|k| !k.todos[0].completed)
        })
    });
    let toast = cx.read(|cx| panel.read(cx).toast.clone());
    assert!(
        toast.as_ref().is_some_and(|toast| toast.error),
        "a failed write toasts: {toast:?}, {:?}",
        server.requests("PATCH", &format!("{}/todos/{id}", base()))
    );

    // Sends leave the panel for the workspace to carry out.
    store.lock().expect("store").fail_writes = false;
    cx.update(|cx| panel.update(cx, |panel, cx| panel.send_todo_to_new(&id, cx)));
    assert!(matches!(
        events.lock().expect("events").last(),
        Some(KnowledgeEvent::Dispatch(crate::knowledge::Dispatch::Todo { text, .. })) if text == "Write release notes"
    ));
    assert_eq!(cx.read(|cx| panel.read(cx).sending.clone()), Some(id));
}

#[gpui_kit::test]
fn a_refresh_started_before_a_note_creation_keeps_the_created_note(cx: &mut TestAppContext) {
    let (server, _) = serve(Store::default());
    let network = Network::new().expect("a network runtime");
    let (_, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));

    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            let before_creation = panel.knowledge.clone().expect("loaded knowledge");
            panel.reload(cx);
            let revision = panel.load_revision;
            let mut created = before_creation.clone();
            created
                .notes
                .push(openchamber_native_core::knowledge::Note {
                    id: "created-note".into(),
                    body: "Keep the new note".into(),
                    created_at: 1,
                    updated_at: 1,
                    source: Default::default(),
                    plan_id: None,
                });
            panel.adopt(created, cx);
            panel.finish_load(revision, Ok(before_creation), cx);
            let notes = &panel.knowledge.as_ref().expect("retained knowledge").notes;
            assert_eq!(
                notes.len(),
                1,
                "an old read cannot erase a confirmed creation"
            );
            assert_eq!(notes[0].id, "created-note");
        });
    });
}

#[gpui_kit::test]
fn a_failed_note_save_keeps_the_draft_and_deletes_wait_for_undo(cx: &mut TestAppContext) {
    let (server, store) = serve(Store {
        notes: vec![
            json!({"id": "n1", "body": "# Release\nShip it", "createdAt": 1, "updatedAt": 1, "source": "manual"}),
        ],
        ..Store::default()
    });
    let network = Network::new().expect("a network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    assert_eq!(cx.read(|cx| panel.read(cx).counts().notes), 1);

    store.lock().expect("store").fail_writes = true;
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_note(Some("n1".into()), window, cx);
            panel.note_editor.update(cx, |input, cx| {
                input.set_value("# Release\nShip it today", window, cx)
            });
            panel.save_note(cx);
        })
    })
    .expect("window is open");
    wait(cx, |cx| cx.read(|cx| panel.read(cx).is_flushed()));
    assert!(
        !server
            .requests("PATCH", &format!("{}/notes/n1", base()))
            .is_empty()
    );
    assert_eq!(
        cx.read(|cx| panel.read(cx).note_editor.read(cx).value().to_string()),
        "# Release\nShip it today",
        "the draft survives the failed save"
    );
    assert!(cx.read(|cx| {
        panel
            .read(cx)
            .toast
            .as_ref()
            .is_some_and(|toast| toast.error)
    }));

    store.lock().expect("store").fail_writes = false;
    cx.update(|cx| panel.update(cx, |panel, cx| panel.save_note(cx)));
    wait(cx, |cx| cx.read(|cx| panel.read(cx).is_flushed()));
    assert_eq!(
        store.lock().expect("store").notes[0]["body"],
        json!("# Release\nShip it today")
    );

    // Undo inside the window sends nothing.
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.schedule_delete(Section::Notes, "n1", window, cx)
        })
    })
    .expect("window is open");
    assert!(cx.read(|cx| panel.read(cx).delete_pending("n1")));
    assert_eq!(
        cx.read(|cx| panel.read(cx).counts().notes),
        0,
        "hidden at once"
    );
    assert!(
        cx.read(|cx| panel.read(cx).open.is_none()),
        "the open note closes"
    );
    cx.update(|cx| panel.update(cx, |panel, cx| panel.undo_delete("n1", cx)));
    assert_eq!(cx.read(|cx| panel.read(cx).counts().notes), 1);
    cx.run_until_parked();
    assert!(
        server
            .requests("DELETE", &format!("{}/notes/n1", base()))
            .is_empty()
    );

    // The window ending sends the delete.
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.schedule_delete(Section::Notes, "n1", window, cx);
            panel.run_delete(Section::Notes, "n1", window, cx);
        })
    })
    .expect("window is open");
    wait(cx, |cx| {
        cx.read(|cx| {
            panel
                .read(cx)
                .knowledge
                .as_ref()
                .is_some_and(|k| k.notes.is_empty())
        })
    });
    assert_eq!(
        server
            .requests("DELETE", &format!("{}/notes/n1", base()))
            .len(),
        1
    );
}

#[gpui_kit::test]
fn plans_open_their_markdown_save_it_and_search_follows(cx: &mut TestAppContext) {
    let (server, _) = serve(Store {
        todos: vec![
            json!({"id": "t1", "text": "Launch checklist", "completed": false, "createdAt": 1}),
        ],
        ..Store::default()
    });
    let network = Network::new().expect("a network runtime");
    let (window, panel, events) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));

    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.toggle_search(window, cx);
            panel
                .search
                .update(cx, |input, cx| input.set_value("launch", window, cx));
            panel.set_query("launch".to_owned(), cx);
        })
    })
    .expect("window is open");
    cx.run_until_parked();
    // Notes has nothing, so the search moves to the first section that has.
    assert_eq!(cx.read(|cx| panel.read(cx).section), Section::Todos);
    let counts = cx.read(|cx| panel.read(cx).counts());
    assert_eq!((counts.notes, counts.todos, counts.plans), (0, 1, 1));

    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.close_search(window, cx);
            panel.select_section(Section::Plans, cx);
            panel.open_plan(Some("p1".into()), window, cx);
        })
    })
    .expect("window is open");
    wait(cx, |cx| {
        matches!(
            cx.read(|cx| panel.read(cx).plan.clone()),
            Some(PlanLoad::Ready(_))
        )
    });
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.plan_editor.update(cx, |input, cx| {
                input.set_value("# Launch\n\n## Steps\n- [x] one\n", window, cx)
            });
            panel.send_plan(true, cx);
        })
    })
    .expect("window is open");
    wait(cx, |_| {
        !server
            .requests("PUT", &format!("{}/plans/p1", base()))
            .is_empty()
    });
    assert_eq!(
        server.requests("PUT", &format!("{}/plans/p1", base()))[0].body["raw"],
        json!("# Launch\n\n## Steps\n- [x] one\n"),
        "Implement saves what is typed first"
    );
    wait(cx, |_| !events.lock().expect("events").is_empty());
    assert!(matches!(
        events.lock().expect("events").last(),
        Some(KnowledgeEvent::Dispatch(crate::knowledge::Dispatch::Plan {
            implement: true,
            ..
        }))
    ));
    let parts = cx.read(|cx| panel.read(cx).plan_send_parts("p1", cx));
    assert_eq!(
        parts.map(|(title, path, _)| (title, path)),
        Some(("Launch".to_owned(), Some("/plans/launch.md".to_owned())))
    );
}
