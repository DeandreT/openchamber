//! Retained editor drafts and the write barriers used by close and quit.

use std::sync::{Arc, Mutex};

use gpui_kit::{AnyWindowHandle, AppContext, Entity, EntityInputHandler, TestAppContext};
use openchamber_native_core::knowledge::{self, Section, Todo};
use openchamber_native_transport::project_context::PlanDocument;
use serde_json::json;

use super::{Store, base, open_panel, serve};
use crate::knowledge::{KnowledgePanel, PlanLoad};
use crate::knowledge_writes::{DocumentDraft, DocumentKey, KnowledgeWriter, SaveOutcome};
use crate::network::Network;
use crate::scheduled_tasks_tests::{Scripted, wait};
use crate::settings_controls::tests::ResponseGate;

fn notes() -> Store {
    Store {
        notes: vec![
            json!({"id": "n1", "body": "Original", "createdAt": 1, "updatedAt": 1, "source": "manual"}),
            json!({"id": "n2", "body": "Other note", "createdAt": 1, "updatedAt": 1, "source": "manual"}),
        ],
        ..Store::default()
    }
}

fn todo(text: &str) -> Todo {
    Todo {
        id: "new-todo".into(),
        text: text.to_owned(),
        completed: false,
        created_at: 1,
        plan_id: None,
        depends_on: Vec::new(),
    }
}

fn edit_note(
    window: AnyWindowHandle,
    panel: &Entity<KnowledgePanel>,
    text: &str,
    cx: &mut TestAppContext,
) {
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel
                .note_editor
                .update(cx, |input, cx| input.set_value(text, window, cx));
            panel.save_note(cx);
        });
    })
    .expect("the editor's window is open");
}

fn held_note_server() -> (Scripted, Arc<Mutex<Store>>, Arc<ResponseGate>) {
    let store = Arc::new(Mutex::new(notes()));
    let shared = store.clone();
    let gate = ResponseGate::new();
    let held = gate.clone();
    let mut first = true;
    let server = Scripted::new(move |request| {
        if first && request.method == "PATCH" {
            first = false;
            held.hold();
        }
        shared
            .lock()
            .expect("store")
            .answer(&request.method, &request.path, &request.body)
    });
    (server, store, gate)
}

#[gpui_kit::test]
fn a_failed_note_draft_survives_visiting_another_entry(cx: &mut TestAppContext) {
    let (server, store) = serve(notes());
    let network = Network::new().expect("a network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    store.lock().expect("store").fail_writes = true;
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_note(Some("n1".into()), window, cx)
        });
    })
    .expect("the window is open");
    edit_note(window, &panel, "Keep this failed draft", cx);
    wait(cx, |cx| {
        cx.read(|cx| panel.read(cx).is_flushed() && !panel.read(cx).loading)
    });
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_note(Some("n2".into()), window, cx)
        });
    })
    .expect("the window is open");
    wait(cx, |cx| {
        cx.read(|cx| panel.read(cx).is_flushed() && !panel.read(cx).loading)
    });
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_note(Some("n1".into()), window, cx)
        });
    })
    .expect("the window is open");
    assert_eq!(
        cx.read(|cx| panel.read(cx).note_editor.read(cx).value().to_string()),
        "Keep this failed draft",
        "opening a different note cannot discard an unsaved entry"
    );
    assert_eq!(store.lock().expect("store").notes[0]["body"], "Original");
}

#[gpui_kit::test]
fn normal_close_drains_the_newest_edit_after_its_pending_save(cx: &mut TestAppContext) {
    let (server, store, gate) = held_note_server();
    let network = Network::new().expect("a network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_note(Some("n1".into()), window, cx)
        });
    })
    .expect("the window is open");
    edit_note(window, &panel, "First edit", cx);
    gate.entered(cx);
    edit_note(window, &panel, "Newest edit", cx);
    let pending = cx
        .update_window(window, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.prepare_close(window, cx);
                panel.take_pending_write(cx)
            })
        })
        .expect("the window is open")
        .expect("close waits for the accepted save");
    assert_eq!(
        cx.read(
            |cx| panel.read(cx).documents[&DocumentKey::Note("n1".into())]
                .pending
                .clone()
        ),
        Some("First edit".into()),
        "the original request retains ownership until its response"
    );
    gate.release();
    cx.foreground_executor().block_test(async move {
        let _ = pending.await;
    });
    assert_eq!(store.lock().expect("store").notes[0]["body"], "Newest edit");
    assert_eq!(
        server
            .requests("PATCH", &format!("{}/notes/n1", base()))
            .len(),
        2
    );
    assert!(cx.read(|cx| panel.read(cx).is_flushed() && !panel.read(cx).has_failed_writes()));
}

#[gpui_kit::test]
fn quit_drains_a_later_edit_without_running_ui_save_completions(cx: &mut TestAppContext) {
    let (server, store, gate) = held_note_server();
    let network = Network::new().expect("a network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_note(Some("n1".into()), window, cx)
        });
    })
    .expect("the window is open");
    edit_note(window, &panel, "First edit", cx);
    gate.entered(cx);
    edit_note(window, &panel, "Newest edit", cx);
    cx.update(|cx| {
        cx.on_app_quit(move |cx| {
            let pending = panel.update(cx, |panel, cx| panel.take_quit_write(cx));
            gate.release();
            async move {
                if let Some(pending) = pending {
                    pending.await;
                }
            }
        })
        .detach();
    });
    cx.executor().allow_parking();
    cx.quit();
    assert_eq!(
        store.lock().expect("store").notes[0]["body"],
        "Newest edit",
        "quit's independent drain persists text waiting behind a UI completion"
    );
    assert_eq!(
        server
            .requests("PATCH", &format!("{}/notes/n1", base()))
            .len(),
        2
    );
}

#[gpui_kit::test]
fn a_failed_plan_save_does_not_confirm_the_accepted_plan_intent(cx: &mut TestAppContext) {
    let (server, store) = serve(Store::default());
    let network = Network::new().expect("a network runtime");
    let (window, panel, events) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_plan(Some("p1".into()), window, cx)
        });
    })
    .expect("the window is open");
    wait(cx, |cx| {
        cx.read(|cx| matches!(panel.read(cx).plan, Some(PlanLoad::Ready(_))))
    });
    store.lock().expect("store").fail_writes = true;
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.plan_editor.update(cx, |input, cx| {
                input.set_value("# Unsaved plan", window, cx)
            });
            panel.send_plan(true, cx);
        });
    })
    .expect("the window is open");
    wait(cx, |cx| cx.read(|cx| panel.read(cx).is_flushed()));
    assert_eq!(
        events.lock().expect("events").len(),
        1,
        "the panel emits one accepted intent; the connected actor gates network creation"
    );
    assert!(
        cx.read(|cx| {
            let panel = panel.read(cx);
            let draft = &panel.documents[&DocumentKey::Plan("p1".into())];
            draft.pending.is_none() && draft.dirty() && draft.saved != "# Unsaved plan"
        }),
        "failed persistence cannot confirm the accepted plan text"
    );
    assert_eq!(
        cx.read(|cx| panel.read(cx).plan_editor.read(cx).value().to_string()),
        "# Unsaved plan"
    );
}

#[gpui_kit::test]
fn returning_to_the_saved_plan_after_a_failed_edit_can_dispatch(cx: &mut TestAppContext) {
    let (server, store) = serve(Store::default());
    let network = Network::new().expect("a network runtime");
    let (window, panel, events) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_plan(Some("p1".into()), window, cx)
        });
    })
    .expect("the window is open");
    wait(cx, |cx| {
        cx.read(|cx| matches!(panel.read(cx).plan, Some(PlanLoad::Ready(_))))
    });
    let original = cx.read(|cx| panel.read(cx).plan_editor.read(cx).value().to_string());
    store.lock().expect("store").fail_writes = true;
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel
                .plan_editor
                .update(cx, |input, cx| input.set_value("# Failed edit", window, cx));
            panel.save_plan(cx);
        });
    })
    .expect("the window is open");
    wait(cx, |cx| cx.read(|cx| panel.read(cx).is_flushed()));
    store.lock().expect("store").fail_writes = false;
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel
                .plan_editor
                .update(cx, |input, cx| input.set_value(original, window, cx));
            panel.send_plan(true, cx);
        });
    })
    .expect("the window is open");
    cx.run_until_parked();
    assert_eq!(
        events.lock().expect("events").len(),
        1,
        "returning to an already persisted plan does not require a new successful write"
    );
}

#[gpui_kit::test]
fn a_reload_cannot_erase_a_todo_mutation_while_its_write_is_pending(cx: &mut TestAppContext) {
    let (server, _) = serve(Store::default());
    let network = Network::new().expect("a network runtime");
    let (_, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            let old = panel.knowledge.clone().expect("loaded knowledge");
            panel.reload(cx);
            let read = panel.load_revision;
            let mutation = panel.begin_mutation();
            let todo = todo("Keep the new task");
            panel
                .knowledge
                .as_mut()
                .expect("loaded knowledge")
                .todos
                .push(todo);
            panel.finish_load(read, Ok(old), cx);
            assert_eq!(
                panel.knowledge.as_ref().expect("knowledge").todos.len(),
                1,
                "an earlier read cannot erase a pending optimistic change"
            );
            panel.finish_mutation(mutation, None, false, cx);
        })
    });
}

#[gpui_kit::test]
fn overlapping_mutation_snapshots_reconcile_once_after_all_writes(cx: &mut TestAppContext) {
    let (server, store) = serve(notes());
    let network = Network::new().expect("a network runtime");
    let (_, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    {
        let mut store = store.lock().expect("store");
        store.notes[0]["body"] = json!("New note text");
        store.todos.push(json!({"id": "new-todo", "text": "Keep both writes", "completed": false, "createdAt": 1}));
    }
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            let original = panel.knowledge.clone().expect("loaded knowledge");
            let note_revision = panel.begin_mutation();
            let mut note_snapshot = original.clone();
            note_snapshot.notes[0].body = "New note text".into();
            panel.knowledge.as_mut().expect("knowledge").notes[0].body = "New note text".into();
            let todo_revision = panel.begin_mutation();
            let mut todo_snapshot = original;
            todo_snapshot.todos.push(todo("Keep both writes"));
            panel
                .knowledge
                .as_mut()
                .expect("knowledge")
                .todos
                .clone_from(&todo_snapshot.todos);
            panel.finish_mutation(todo_revision, Some(todo_snapshot), false, cx);
            panel.finish_mutation(note_revision, Some(note_snapshot), false, cx);
            let current = panel.knowledge.as_ref().expect("knowledge");
            assert_eq!(
                current.notes[0].body, "New note text",
                "an older snapshot cannot replace a later note mutation"
            );
            assert_eq!(
                current.todos.len(),
                1,
                "an older snapshot cannot erase a newer todo mutation"
            );
        })
    });
    wait(cx, |cx| cx.read(|cx| !panel.read(cx).loading));
    cx.read(|cx| {
        let current = panel.read(cx).knowledge.as_ref().expect("knowledge");
        assert_eq!(current.notes[0].body, "New note text");
        assert_eq!(current.todos.len(), 1);
    });
    assert_eq!(
        server.requests("GET", &base()).len(),
        2,
        "overlapping completions coalesce into one authoritative refresh"
    );
}

#[gpui_kit::test]
fn a_save_reply_never_replaces_another_open_plans_metadata(cx: &mut TestAppContext) {
    let (server, _) = serve(Store::default());
    let network = Network::new().expect("a network runtime");
    let (_, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            let original = panel.knowledge.as_ref().expect("knowledge").plans[0].clone();
            let mut other = original.clone();
            other.id = "p2".into();
            other.title = "Another plan".into();
            panel
                .knowledge
                .as_mut()
                .expect("knowledge")
                .plans
                .push(other.clone());
            for outcome in [SaveOutcome::Plan(original), SaveOutcome::Missing] {
                let key = DocumentKey::Plan("p1".into());
                let mut draft = DocumentDraft::new("old text".into());
                draft.text = "saved text".into();
                let sent = draft.begin().expect("a changed draft");
                panel.documents.insert(key.clone(), draft);
                let revision = panel.begin_mutation();
                panel.open = Some((Section::Plans, other.id.clone()));
                panel.plan = Some(PlanLoad::Ready(Box::new(PlanDocument {
                    plan: other.clone(),
                    raw: "Other document".into(),
                    path: Some("/plans/other.md".into()),
                })));
                panel.finish_document(revision, key, sent, outcome, cx);
                let Some(PlanLoad::Ready(document)) = &panel.plan else {
                    panic!("a missing original plan cannot hide another open document");
                };
                assert_eq!(
                    document.plan, other,
                    "save metadata stays with its originating entry"
                );
                assert_eq!(
                    document.raw, "Other document",
                    "the visible editor's saved text is untouched"
                );
            }
        })
    });
}

#[gpui_kit::test]
fn leaving_an_entry_does_not_persist_its_provisional_ime_text(cx: &mut TestAppContext) {
    let (server, store) = serve(notes());
    let network = Network::new().expect("a network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_note(Some("n1".into()), window, cx);
            panel.note_editor.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "未", Some(0..1), window, cx);
                assert!(
                    input.marked_text_range(window, cx).is_some(),
                    "the editor is composing"
                );
            });
            panel.close_entry(cx);
        });
    })
    .expect("the window is open");
    wait(cx, |cx| cx.read(|cx| panel.read(cx).is_flushed()));
    assert!(
        server
            .requests("PATCH", &format!("{}/notes/n1", base()))
            .is_empty(),
        "leaving an entry saves only previously committed edits"
    );
    assert_eq!(store.lock().expect("store").notes[0]["body"], "Original");
}

#[tokio::test]
async fn a_different_saved_plan_body_is_not_confirmed_as_the_captured_text() {
    let server = Scripted::new(|_| {
        (
            200,
            json!({
                "plan": {"id": "p1", "title": "Plan", "createdAt": 1},
                "raw": "The server kept only a prefix"
            }),
        )
    });
    let writer = KnowledgeWriter::new().expect("a write executor");
    let outcome = writer
        .save(
            server.client(),
            "project".into(),
            DocumentKey::Plan("p1".into()),
            "The whole captured plan document".into(),
        )
        .await
        .expect("a save receipt");
    writer.finish().await;
    assert!(
        matches!(outcome, SaveOutcome::Failed),
        "a truncated or changed stored document cannot authorize captured-plan dispatch"
    );
}

#[tokio::test]
async fn a_plan_save_can_omit_returned_contents_per_the_reference_contract() {
    let server = Scripted::new(|_| {
        (
            200,
            json!({
                "plan": {"id": "p1", "title": "Plan", "createdAt": 1}
            }),
        )
    });
    let writer = KnowledgeWriter::new().expect("a write executor");
    let outcome = writer
        .save(
            server.client(),
            "project".into(),
            DocumentKey::Plan("p1".into()),
            "The captured plan document".into(),
        )
        .await
        .expect("a save receipt");
    writer.finish().await;
    assert!(
        matches!(outcome, SaveOutcome::Plan(plan) if plan.id == "p1"),
        "the reference accepts a plan acknowledgment with omitted raw text"
    );
}

#[gpui_kit::test]
fn note_edits_respect_the_reference_utf16_length_boundary(cx: &mut TestAppContext) {
    let (server, _) = serve(notes());
    let network = Network::new().expect("a network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_note(Some("n1".into()), window, cx);
            panel.note_editor.update(cx, |input, cx| {
                let at_limit = "😀".repeat(knowledge::NOTE_BODY_MAX_LENGTH / 2);
                input.replace_text_in_range(Some(0..8), &at_limit, window, cx);
                assert!(
                    input.value().as_str() == at_limit,
                    "the boundary itself stays editable"
                );
                input.replace_text_in_range(None, "a", window, cx);
                assert_eq!(
                    input.value().encode_utf16().count(),
                    knowledge::NOTE_BODY_MAX_LENGTH,
                    "an over-limit edit is rejected before the server could truncate the note"
                );
                assert!(
                    input.value().as_str() == at_limit,
                    "the accepted note text stays intact"
                );
                let selection = input.selected_range();
                input.replace_and_mark_text_in_range(None, "x", Some(1..1), window, cx);
                assert!(
                    input.value().as_str() == at_limit,
                    "IME edits obey the same limit"
                );
                assert!(
                    input.marked_text_range(window, cx).is_none(),
                    "a refused IME edit leaves no preedit"
                );
                assert_eq!(
                    input.selected_range(),
                    selection,
                    "a refused edit keeps the caret"
                );
                input.replace_text_in_range(Some(0..2), "a", window, cx);
                assert_eq!(
                    input.value().encode_utf16().count(),
                    knowledge::NOTE_BODY_MAX_LENGTH - 1,
                    "replacing an emoji with a shorter character remains editable at the limit"
                );
            });
        });
    })
    .expect("the window is open");
}

#[test]
fn knowledge_input_clipping_counts_server_utf16_units_without_splitting_unicode() {
    for (text, limit, expected) in [
        ("a😀b", 2, "a"),
        ("a😀b", 3, "a😀"),
        ("😀\r\nb", 3, "😀 "),
        ("😀", 1, ""),
        ("x", 0, ""),
    ] {
        assert_eq!(
            knowledge::single_line(text, limit),
            expected,
            "native input must obey the server's string-length boundary for {text:?}"
        );
    }
}

fn failed_creation_refuses_close(plan: bool, cx: &mut TestAppContext) {
    let store = Arc::new(Mutex::new(notes()));
    let shared = Arc::clone(&store);
    let gate = ResponseGate::new();
    let held = Arc::clone(&gate);
    let server = Scripted::new(move |request| {
        if request.method == "POST" {
            held.hold();
            return (500, json!({"error": "disk full"}));
        }
        shared
            .lock()
            .expect("store")
            .answer(&request.method, &request.path, &request.body)
    });
    let network = Network::new().expect("a network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            if plan {
                panel.plan_input.update(cx, |input, cx| {
                    input.set_value("Keep this plan title", window, cx)
                });
                panel.add_plan(window, cx);
            } else {
                panel.note_input.update(cx, |input, cx| {
                    input.set_value("Keep this note submission", window, cx)
                });
                panel.add_note(window, cx);
            }
        });
    })
    .expect("the creation field is available");
    gate.entered(cx);
    let pending = cx
        .update_window(window, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.prepare_close(window, cx);
                panel.take_pending_write(cx)
            })
        })
        .expect("the window is open")
        .expect("the accepted creation must drain");
    gate.release();
    cx.foreground_executor().block_test(async move {
        pending.await.expect("the creation finishes");
    });
    assert!(
        cx.read(|cx| panel.read(cx).has_failed_writes()),
        "a failed submitted creation must keep its restored text available before close"
    );
    cx.update(|cx| panel.update(cx, |panel, cx| panel.cancel_close(cx)));
    cx.read(|cx| {
        let panel = panel.read(cx);
        let actual = if plan {
            panel.plan_input.read(cx).value()
        } else {
            panel.note_input.read(cx).value()
        };
        assert!(
            !actual.is_empty(),
            "the failed submission remains recoverable"
        );
    });
}

#[gpui_kit::test]
fn failed_note_creation_refuses_close(cx: &mut TestAppContext) {
    failed_creation_refuses_close(false, cx);
}

#[gpui_kit::test]
fn failed_plan_creation_refuses_close(cx: &mut TestAppContext) {
    failed_creation_refuses_close(true, cx);
}

#[gpui_kit::test]
fn a_plan_read_started_during_a_save_cannot_replace_its_completed_text(cx: &mut TestAppContext) {
    let gate = ResponseGate::new();
    let held = Arc::clone(&gate);
    let mut store = Store::default();
    let server = Scripted::new(move |request| {
        if request.method == "PUT" {
            held.hold();
        }
        store.answer(&request.method, &request.path, &request.body)
    });
    let network = Network::new().expect("a network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_plan(Some("p1".into()), window, cx)
        });
    })
    .expect("the window is open");
    wait(cx, |cx| {
        cx.read(|cx| matches!(panel.read(cx).plan, Some(PlanLoad::Ready(_))))
    });
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.plan_editor.update(cx, |input, cx| {
                input.set_value("# The newest saved plan", window, cx)
            });
            panel.save_plan(cx);
        });
    })
    .expect("the editor is open");
    gate.entered(cx);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.close_entry(cx);
            panel.open_plan(Some("p1".into()), window, cx);
        });
    })
    .expect("the same plan is reopened while its save is pending");
    gate.release();
    wait(cx, |cx| {
        cx.read(|cx| {
            panel.read(cx).is_flushed() && matches!(panel.read(cx).plan, Some(PlanLoad::Ready(_)))
        })
    });
    assert_eq!(
        cx.read(|cx| panel.read(cx).plan_editor.read(cx).value().to_string()),
        "# The newest saved plan",
        "the late old read cannot undo the completed save"
    );
}

#[gpui_kit::test]
fn note_creation_respects_the_reference_utf16_boundary(cx: &mut TestAppContext) {
    let (server, _) = serve(notes());
    let network = Network::new().expect("a network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.note_input.update(cx, |input, cx| {
                let at_limit = "😀".repeat(knowledge::NOTE_BODY_MAX_LENGTH / 2);
                input.replace_text_in_range(None, &at_limit, window, cx);
                assert_eq!(
                    input.value().as_str(),
                    at_limit,
                    "the full valid note remains editable"
                );
                input.replace_text_in_range(None, "x", window, cx);
                assert_eq!(
                    input.value().encode_utf16().count(),
                    knowledge::NOTE_BODY_MAX_LENGTH,
                    "note creation cannot submit text the server would truncate"
                );
                input.replace_text_in_range(Some(0..2), "a", window, cx);
                assert_eq!(
                    input.value().encode_utf16().count(),
                    knowledge::NOTE_BODY_MAX_LENGTH - 1,
                    "shortening a boundary emoji remains editable"
                );
            });
        });
    })
    .expect("the creation input is open");
}

#[gpui_kit::test]
fn an_expired_delete_uses_the_client_captured_before_its_owner_disconnected(
    cx: &mut TestAppContext,
) {
    let (server, store) = serve(notes());
    let network = Network::new().expect("a network runtime");
    let (window, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    let (_, disconnected_owner) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.schedule_delete(Section::Notes, "n1", window, cx);
            // The live scope can no longer resolve an active connection, as
            // after a host switch. The accepted delete retains its original one.
            panel.scope.owner = Some(disconnected_owner.downgrade());
            panel.run_delete(Section::Notes, "n1", window, cx);
        });
    })
    .expect("the undo timer's owning window remains open");
    wait(cx, |cx| cx.read(|cx| panel.read(cx).is_flushed()));
    assert_eq!(
        server
            .requests("DELETE", &format!("{}/notes/n1", base()))
            .len(),
        1,
        "the expired accepted delete keeps its original host and project"
    );
    assert!(store.lock().expect("store").notes.is_empty());
}

#[gpui_kit::test]
fn externally_removed_dirty_documents_can_be_copied_and_explicitly_discarded(
    cx: &mut TestAppContext,
) {
    let (server, _) = serve(notes());
    let network = Network::new().expect("a network runtime");
    let (_, panel, _) = open_panel(cx, &server, &network);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    for key in [
        DocumentKey::Note("n1".into()),
        DocumentKey::Plan("p1".into()),
    ] {
        cx.update(|cx| {
            panel.update(cx, |panel, cx| {
                let mut draft = DocumentDraft::new("Stored before removal".into());
                draft.set_text("Keep the exact unsaved text 😀\nSecond line".into());
                let sent = draft.begin().expect("the changed document starts a save");
                panel.documents.insert(key.clone(), draft);
                let revision = panel.begin_mutation();
                panel.finish_document(revision, key.clone(), sent, SaveOutcome::Missing, cx);
                assert!(
                    panel.has_failed_writes(),
                    "missing unsaved text must hold normal close"
                );
                assert!(
                    panel
                        .deleted_drafts()
                        .iter()
                        .any(|(entry, text)| **entry == key
                            && *text == "Keep the exact unsaved text 😀\nSecond line"),
                    "a missing document remains reachable through recovery"
                );
                panel.copy_deleted_draft(&key, cx);
            });
        });
        assert_eq!(
            cx.read(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
                .as_deref(),
            Some("Keep the exact unsaved text 😀\nSecond line"),
            "Copy preserves the complete original draft bytes"
        );
        cx.update(|cx| {
            panel.update(cx, |panel, cx| {
                panel.discard_deleted_draft(&key, cx);
                assert!(
                    !panel.has_failed_writes(),
                    "explicit local discard releases the close barrier"
                );
            })
        });
    }
    assert!(
        server
            .requests("POST", &format!("{}/notes", base()))
            .is_empty(),
        "recovery cannot recreate a deleted note"
    );
    assert!(
        server
            .requests("POST", &format!("{}/plans", base()))
            .is_empty(),
        "recovery cannot recreate a deleted plan"
    );
}
