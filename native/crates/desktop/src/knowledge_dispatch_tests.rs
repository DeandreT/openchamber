//! Accepted sends retain their captured I/O through close and final transfer.

use std::sync::{Arc, Mutex};

use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_core::knowledge::Section;
use openchamber_native_core::send_config::{ModelChoice, Selection};
use openchamber_native_transport::client::ClientDescription;
use serde_json::{Value, json};

use super::{KnowledgeOwner, KnowledgePanel, PlanLoad};
use crate::connection::{Connection, ServerConfig};
use crate::files_view::PanelScope;
use crate::scheduled_tasks_tests::{Request, Scripted, wait};
use crate::send_catalog::{DirectoryCatalog, Load};
use crate::settings_controls::tests::ResponseGate;
use crate::workspace::Workspace;

const ROOT: &str = "/work/project";

#[derive(Clone, Copy)]
enum SaveReply {
    Exact,
    Omitted,
    Mismatch,
    Failed,
    FailSecond,
}

fn fixture(
    reply: SaveReply,
    hold: Option<(String, Arc<ResponseGate>)>,
) -> (Scripted, Arc<Mutex<Vec<Request>>>) {
    let operations = Arc::new(Mutex::new(Vec::new()));
    let captured = operations.clone();
    let mut saves = 0;
    let server = Scripted::new(move |request| {
        captured.lock().expect("request log").push(request.clone());
        if let Some((path, gate)) = &hold
            && request.method != "GET"
            && request.path.ends_with(path)
        {
            gate.hold();
        }
        let plan = json!({"id": "p1", "title": "Launch", "createdAt": 1});
        let context = json!({
            "notes": [],
            "todos": [{"id": "t1", "text": "Captured task", "completed": false, "createdAt": 1}],
            "plans": [plan], "sharedPlansDir": null
        });
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/api/routing") => (
                200,
                json!({"available": true, "autoReady": true, "jevAvailable": false}),
            ),
            ("GET", "/api/magic-prompts") => (200, json!({"version": 1, "overrides": {}})),
            ("GET", path) if path.ends_with("/plans/p1") => (
                200,
                json!({"id": "p1", "title": "Launch", "createdAt": 1,
                    "raw": "Original plan", "path": null}),
            ),
            ("GET", path) if path.starts_with("/api/project-context/") => (200, context),
            ("PUT", path) if path.ends_with("/plans/p1/session") => {
                (200, json!({"plan": plan, "context": context}))
            }
            ("PUT", path) if path.ends_with("/plans/p1") => {
                saves += 1;
                match reply {
                    SaveReply::Exact => (200, json!({"plan": plan, "raw": request.body["raw"]})),
                    SaveReply::Omitted => (200, json!({"plan": plan})),
                    SaveReply::Mismatch => (200, json!({"plan": plan, "raw": "Different text"})),
                    SaveReply::Failed => (500, json!({"error": "fixture save failure"})),
                    SaveReply::FailSecond if saves == 2 => {
                        (500, json!({"error": "second save failed"}))
                    }
                    SaveReply::FailSecond => {
                        (200, json!({"plan": plan, "raw": request.body["raw"]}))
                    }
                }
            }
            ("POST", "/api/session") => (
                200,
                json!({"data": {"id": request.body["id"], "location": {"directory": ROOT}, "title": "Captured send",
                    "time": {"created": 1, "updated": 1}}}),
            ),
            ("POST", path) if path.ends_with("/model") || path.ends_with("/agent") => {
                (204, Value::Null)
            }
            ("POST", path) if path.ends_with("/synthetic") || path.ends_with("/prompt") => {
                let session = path.split('/').nth(3).expect("session route");
                let kind = if path.ends_with("/synthetic") {
                    "synthetic"
                } else {
                    "user"
                };
                (
                    200,
                    json!({"data": {"id": request.body["id"], "sessionID": session,
                    "type": kind, "time": {"created": 1}, "delivery": "queue", "payload": {}}}),
                )
            }
            _ => (404, json!({"error": "unexpected fixture route"})),
        }
    });
    (server, operations)
}

fn open(
    server: &Scripted,
    cx: &mut TestAppContext,
) -> (AnyWindowHandle, Entity<Workspace>, Entity<KnowledgePanel>) {
    open_with_model(server, "fixture", "captured-model", cx)
}

fn open_with_model(
    server: &Scripted,
    provider: &str,
    model: &str,
    cx: &mut TestAppContext,
) -> (AnyWindowHandle, Entity<Workspace>, Entity<KnowledgePanel>) {
    cx.executor().allow_parking();
    let (window, workspace) = crate::workspace_tests::open(cx);
    let panel = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let session = workspace
                    .sessions
                    .active()
                    .first()
                    .expect("synthetic session")
                    .id
                    .clone();
                workspace.select_session(session, window, cx);
                let directory = workspace
                    .config_directory()
                    .expect("selected config directory");
                let mut catalog = DirectoryCatalog::default();
                catalog.agents = Load::Ready(Vec::new());
                catalog.providers = Load::Ready(Vec::new());
                catalog.opencode = Load::Ready(Default::default());
                workspace
                    .send_catalog
                    .insert_test_directory(directory, catalog);
                workspace.send_catalog.routing =
                    openchamber_native_core::send_config::AutoRouting::Ready;
                let owner = workspace
                    .selection_owner()
                    .expect("composer selection owner");
                workspace.composer_selections = Default::default();
                workspace.composer_selections.restore_if_absent(
                    owner,
                    Selection {
                        model: Some(ModelChoice {
                            provider_id: provider.into(),
                            model_id: model.into(),
                            variant: Some("high".into()),
                        }),
                        agent_name: Some("captured-agent".into()),
                    },
                );
                let mut connection = Connection::new(ServerConfig {
                    endpoint: server.endpoint.clone(),
                    password: None,
                    client: ClientDescription::default(),
                })
                .expect("fixture connection");
                connection.client = Some(server.client());
                let scope = PanelScope {
                    client: server.client(),
                    network: connection.network.handle(),
                    runtime: workspace.runtime.clone(),
                    directory: ROOT.into(),
                    owner: None,
                };
                workspace.connection = Some(connection);
                let owner = KnowledgeOwner::new(ROOT, "project".into()).expect("knowledge owner");
                let panel = cx.new(|cx| KnowledgePanel::new(scope, owner, window, cx));
                cx.subscribe_in(&panel, window, |workspace, panel, event, window, cx| {
                    workspace.on_knowledge_event(panel, event, window, cx);
                })
                .detach();
                panel
            })
        })
        .expect("fixture window");
    wait(cx, |cx| cx.read(|cx| panel.read(cx).knowledge.is_some()));
    (window, workspace, panel)
}

fn implement(window: AnyWindowHandle, panel: &Entity<KnowledgePanel>, cx: &mut TestAppContext) {
    load_plan(window, panel, cx);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.plan_editor.update(cx, |input, cx| {
                input.set_value("Captured whole plan", window, cx)
            });
            panel.send_plan(true, cx);
        });
    })
    .expect("fixture window");
}

fn load_plan(window: AnyWindowHandle, panel: &Entity<KnowledgePanel>, cx: &mut TestAppContext) {
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.select_section(Section::Plans, cx);
            panel.open_plan(Some("p1".into()), window, cx);
        });
    })
    .expect("fixture window");
    wait(cx, |cx| {
        cx.read(|cx| matches!(panel.read(cx).plan, Some(PlanLoad::Ready(_))))
    });
}

fn assert_one_send(server: &Scripted) {
    let creates = server.requests("POST", "/api/session");
    assert_eq!(
        creates.len(),
        1,
        "one accepted intent creates exactly one session"
    );
    let session = creates[0].body["id"].as_str().expect("captured session ID");
    assert_eq!(
        creates[0].body["location"]["directory"], ROOT,
        "the owner stays captured"
    );
    assert_eq!(
        creates[0].body["model"],
        json!({"providerID": "fixture", "id": "captured-model", "variant": "high"}),
        "creation retains the accepted model and variant"
    );
    assert_eq!(
        creates[0].body["agent"], "captured-agent",
        "creation retains the accepted agent"
    );
    let model = server.requests("POST", &format!("/api/session/{session}/model"));
    let agent = server.requests("POST", &format!("/api/session/{session}/agent"));
    assert_eq!(model.len(), 1, "selection occurs once");
    assert_eq!(agent.len(), 1, "agent selection occurs once");
    assert_eq!(
        model[0].body,
        json!({"model": {"providerID": "fixture", "id": "captured-model", "variant": "high"}})
    );
    assert_eq!(agent[0].body, json!({"agent": "captured-agent"}));
    let context = server.requests("POST", &format!("/api/session/{session}/synthetic"));
    let prompt = server.requests("POST", &format!("/api/session/{session}/prompt"));
    assert_eq!(context.len(), 1, "synthetic context is admitted once");
    assert_eq!(prompt.len(), 1, "the prompt is admitted once");
    assert_ne!(
        context[0].body["id"], prompt[0].body["id"],
        "message identities stay distinct"
    );
}

#[gpui_kit::test]
fn accepted_todo_joins_close_and_survives_connection_drop(cx: &mut TestAppContext) {
    let gate = ResponseGate::new();
    let (server, _) = fixture(
        SaveReply::Exact,
        Some(("/api/session".into(), gate.clone())),
    );
    let (_, workspace, panel) = open(&server, cx);
    cx.update(|cx| panel.update(cx, |panel, cx| panel.send_todo_to_new("t1", cx)));
    gate.entered(cx);
    let pending = cx
        .update(|cx| panel.update(cx, |panel, cx| panel.take_pending_write(cx)))
        .expect("normal close waits for an accepted todo send");
    cx.update(|cx| {
        workspace.update(cx, |workspace, _| {
            workspace.composer_selections = Default::default();
            workspace.connection = None;
            workspace.runtime = RuntimeId::from("a-different-runtime");
        })
    });
    gate.release();
    cx.foreground_executor().block_test(async move {
        let _ = pending.await;
    });
    assert_one_send(&server);
    assert!(
        cx.read(|cx| panel.read(cx).is_flushed()),
        "the close barrier settles"
    );
}

#[gpui_kit::test]
fn accepted_implementation_transfers_pending_save_and_send_without_ui_callbacks(
    cx: &mut TestAppContext,
) {
    let gate = ResponseGate::new();
    let (server, operations) =
        fixture(SaveReply::Omitted, Some(("/plans/p1".into(), gate.clone())));
    let (window, workspace, panel) = open(&server, cx);
    implement(window, &panel, cx);
    gate.entered(cx);
    assert!(
        server.requests("POST", "/api/session").is_empty(),
        "save gates session creation"
    );
    cx.update(|cx| {
        cx.on_app_quit(move |cx| {
            let pending = panel.update(cx, |panel, cx| panel.take_quit_write(cx));
            workspace.update(cx, |workspace, _| workspace.connection = None);
            gate.release();
            async move {
                if let Some(pending) = pending {
                    pending.await;
                }
            }
        })
        .detach();
    });
    cx.quit();
    assert_one_send(&server);
    let operations = operations.lock().expect("request log");
    let save = operations
        .iter()
        .position(|request| request.method == "PUT" && request.path.ends_with("/plans/p1"))
        .expect("captured plan save");
    let create = operations
        .iter()
        .position(|request| request.method == "POST" && request.path == "/api/session")
        .expect("session creation");
    assert!(save < create, "the persisted plan precedes creation");
    assert_eq!(operations[save].body["raw"], "Captured whole plan");
    let context = operations
        .iter()
        .find(|request| request.path.ends_with("/synthetic"))
        .expect("context request");
    assert!(
        context.body["text"]
            .as_str()
            .expect("context text")
            .contains("Captured whole plan"),
        "a plan without a path retains its exact raw fallback"
    );
    assert_eq!(
        operations
            .iter()
            .filter(|request| request.method == "PUT" && request.path.ends_with("/plans/p1"))
            .count(),
        1,
        "UI and dispatch share one accepted save"
    );
}

fn refused_plan(reply: SaveReply, cx: &mut TestAppContext) {
    let (server, _) = fixture(reply, None);
    let (window, _, panel) = open(&server, cx);
    implement(window, &panel, cx);
    wait(cx, |cx| cx.read(|cx| panel.read(cx).is_flushed()));
    assert!(
        server.requests("POST", "/api/session").is_empty(),
        "an unconfirmed plan cannot create a session"
    );
    assert!(
        cx.read(|cx| panel.read(cx).has_failed_writes()),
        "the captured draft stays unsaved"
    );
    assert_eq!(
        cx.read(|cx| panel.read(cx).plan_editor.read(cx).value().to_string()),
        "Captured whole plan"
    );
}

#[gpui_kit::test]
fn failed_plan_persistence_cannot_start_a_session(cx: &mut TestAppContext) {
    refused_plan(SaveReply::Failed, cx);
}

#[gpui_kit::test]
fn explicit_plan_body_mismatch_cannot_start_a_session(cx: &mut TestAppContext) {
    refused_plan(SaveReply::Mismatch, cx);
}

fn transfer_existing_save(previous: &str, expected_saves: usize, cx: &mut TestAppContext) {
    let gate = ResponseGate::new();
    let (server, operations) = fixture(SaveReply::Exact, Some(("/plans/p1".into(), gate.clone())));
    let (window, workspace, panel) = open(&server, cx);
    load_plan(window, &panel, cx);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel
                .plan_editor
                .update(cx, |input, cx| input.set_value(previous, window, cx));
            panel.save_plan(cx);
        });
    })
    .expect("fixture window");
    gate.entered(cx);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.plan_editor.update(cx, |input, cx| {
                input.set_value("Captured whole plan", window, cx)
            });
            panel.send_plan(true, cx);
        });
    })
    .expect("fixture window");
    cx.run_until_parked();
    assert!(
        cx.read(|cx| panel.read(cx).sending.as_deref() == Some("p1")),
        "the send is accepted before save completion"
    );
    cx.update(|cx| {
        cx.on_app_quit(move |cx| {
            let pending = panel.update(cx, |panel, cx| panel.take_quit_write(cx));
            workspace.update(cx, |workspace, _| workspace.connection = None);
            gate.release();
            async move {
                if let Some(pending) = pending {
                    pending.await;
                }
            }
        })
        .detach();
    });
    cx.quit();
    assert_one_send(&server);
    let operations = operations.lock().expect("request log");
    let saves: Vec<_> = operations
        .iter()
        .filter(|request| request.method == "PUT" && request.path.ends_with("/plans/p1"))
        .collect();
    assert_eq!(
        saves.len(),
        expected_saves,
        "accepted persistence is never duplicated"
    );
    assert_eq!(
        saves.last().expect("last save").body["raw"],
        "Captured whole plan"
    );
    let create = operations
        .iter()
        .position(|request| request.path == "/api/session")
        .expect("creation");
    assert_eq!(
        operations[..create]
            .iter()
            .filter(|request| request.method == "PUT" && request.path.ends_with("/plans/p1"))
            .count(),
        expected_saves,
        "all captured saves precede the dispatch job that awaits them"
    );
}

#[gpui_kit::test]
fn implementation_reuses_its_existing_exact_save_during_final_transfer(cx: &mut TestAppContext) {
    transfer_existing_save("Captured whole plan", 1, cx);
}

#[gpui_kit::test]
fn implementation_queues_newer_captured_text_before_its_dispatch_during_final_transfer(
    cx: &mut TestAppContext,
) {
    transfer_existing_save("Older in-flight plan", 2, cx);
}

#[gpui_kit::test]
fn auto_is_omitted_only_from_creation_and_retained_for_the_accepted_prompt(
    cx: &mut TestAppContext,
) {
    let gate = ResponseGate::new();
    let (server, _) = fixture(
        SaveReply::Exact,
        Some(("/api/session".into(), gate.clone())),
    );
    let (_, workspace, panel) = open_with_model(&server, "openchamber", "auto", cx);
    cx.read(|cx| {
        let (_, captured) = workspace
            .read(cx)
            .send_selection()
            .expect("the fixture authoritatively supports the captured Auto selection");
        assert_eq!(
            captured,
            Selection {
                model: Some(ModelChoice {
                    provider_id: "openchamber".into(),
                    model_id: "auto".into(),
                    variant: Some("high".into()),
                }),
                agent_name: Some("captured-agent".into()),
            },
            "the send starts with a real captured Auto, effort and agent selection"
        );
    });
    cx.update(|cx| panel.update(cx, |panel, cx| panel.send_todo_to_new("t1", cx)));
    gate.entered(cx);
    cx.update(|cx| {
        workspace.update(cx, |workspace, _| {
            let owner = workspace.selection_owner().expect("live composer owner");
            workspace.composer_selections = Default::default();
            workspace.composer_selections.restore_if_absent(
                owner,
                Selection {
                    model: Some(ModelChoice {
                        provider_id: "later".into(),
                        model_id: "different".into(),
                        variant: None,
                    }),
                    agent_name: Some("later-agent".into()),
                },
            );
        })
    });
    gate.release();
    wait(cx, |cx| cx.read(|cx| panel.read(cx).is_flushed()));
    let creates = server.requests("POST", "/api/session");
    assert_eq!(
        creates.len(),
        1,
        "Auto creates one session under the captured identity"
    );
    assert!(
        creates[0].body.get("model").is_none(),
        "creation omits the synthetic Auto model"
    );
    assert_eq!(creates[0].body["agent"], "captured-agent");
    let session = creates[0].body["id"].as_str().expect("captured session ID");
    let models = server.requests("POST", &format!("/api/session/{session}/model"));
    let agents = server.requests("POST", &format!("/api/session/{session}/agent"));
    assert_eq!(models.len(), 1);
    assert_eq!(
        models[0].body,
        json!({"model": {"providerID": "openchamber", "id": "auto", "variant": "high"}}),
        "the prompt retains captured Auto and its effort after a composer change"
    );
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].body, json!({"agent": "captured-agent"}));
    assert_eq!(
        server
            .requests("POST", &format!("/api/session/{session}/prompt"))
            .len(),
        1,
        "configuration changes cannot duplicate admission"
    );
}

#[gpui_kit::test]
fn a_superseded_success_remains_the_saved_baseline_after_implementation_save_failure(
    cx: &mut TestAppContext,
) {
    let gate = ResponseGate::new();
    let (server, operations) = fixture(
        SaveReply::FailSecond,
        Some(("/plans/p1".into(), gate.clone())),
    );
    let (window, _, panel) = open(&server, cx);
    load_plan(window, &panel, cx);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel
                .plan_editor
                .update(cx, |input, cx| input.set_value("Persisted A", window, cx));
            panel.save_plan(cx);
        });
    })
    .expect("fixture window");
    gate.entered(cx);
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel
                .plan_editor
                .update(cx, |input, cx| input.set_value("Rejected B", window, cx));
            panel.send_plan(true, cx);
        });
    })
    .expect("fixture window");
    gate.release();
    wait(cx, |cx| {
        cx.read(|cx| panel.read(cx).is_flushed() && panel.read(cx).sending.is_none())
    });
    assert!(
        server.requests("POST", "/api/session").is_empty(),
        "failed captured persistence blocks implementation"
    );
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel
                .plan_editor
                .update(cx, |input, cx| input.set_value("Original plan", window, cx));
            panel.prepare_close(window, cx);
        });
    })
    .expect("fixture window");
    wait(cx, |cx| cx.read(|cx| panel.read(cx).is_flushed()));
    let operations = operations.lock().expect("request log");
    let saves: Vec<_> = operations
        .iter()
        .filter(|request| request.method == "PUT" && request.path.ends_with("/plans/p1"))
        .collect();
    assert_eq!(
        saves.len(),
        3,
        "closing must persist the restored original after A succeeded and B failed"
    );
    assert_eq!(saves[0].body["raw"], "Persisted A");
    assert_eq!(saves[1].body["raw"], "Rejected B");
    assert_eq!(
        saves[2].body["raw"], "Original plan",
        "the last accepted edit replaces the server's successful A"
    );
    assert!(
        !cx.read(|cx| panel.read(cx).has_failed_writes()),
        "the restored original is confirmed before close"
    );
}
