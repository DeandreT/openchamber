//! End-to-end state tests with the real journal and a controlled HTTP server.

use openchamber_native_core::ids::ProjectId;
use openchamber_native_core::ownership::ConfiguredProject;
use serde_json::json;

use crate::create_attempt::{CreationCheck, CreationPhase};
use crate::send_attempt::SendOrigin;

use super::*;

#[path = "unit_d_creation_close_tests.rs"]
mod unit_d_close_tests;

#[derive(Clone, Copy)]
enum Outcome {
    Success,
    Lost,
    WrongId,
    WrongDirectory,
    Rejected,
}

fn creation_server(journal: PathBuf, outcome: Outcome, hold: bool) -> (Server, Arc<AtomicBool>) {
    creation_server_with_holds(journal, outcome, hold, Vec::new())
}

fn creation_server_with_holds(
    journal: PathBuf,
    outcome: Outcome,
    hold: bool,
    holds: Vec<super::unit_d_close_support::HeldResponse>,
) -> (Server, Arc<AtomicBool>) {
    let (server, missing, _) =
        creation_server_with_native(journal, outcome, hold, holds, None, false, false);
    (server, missing)
}

fn session_first_state(scope: Value, draft: bool, invalid: bool) -> Value {
    let mut state: Value = serde_json::from_str::<Value>(include_str!(
        "../../../../packages/sdk/fixtures/native-agent-v1.json"
    ))
    .expect("SDK fixture")["sessionFirstDraft"]
        .clone();
    state["scope"] = scope;
    state["activationID"] = json!(if draft {
        "draft-config"
    } else {
        "real-runtime"
    });
    state["config"] = json!([{ "id":"runtime", "label":"Runtime", "role":"mode", "value":if draft { "cloud" } else { "local" }, "choices":[{"id":"local","label":"Local"},{"id":"cloud","label":"Cloud"}] }]);
    if draft {
        state["config"].as_array_mut().expect("config").push(json!({"id":"effort","label":"Effort","role":"effort","value":if invalid {"retired"} else {"deep"},"choices":[{"id":"deep","label":"Deep"},{"id":"retired","label":"Retired"}]}));
    }
    state
}

fn creation_server_with_native(
    journal: PathBuf,
    outcome: Outcome,
    hold: bool,
    holds: Vec<super::unit_d_close_support::HeldResponse>,
    session_first: Option<bool>,
    authenticate: bool,
    replace_agent_after_auth: bool,
) -> (Server, Arc<AtomicBool>, Arc<std::sync::atomic::AtomicUsize>) {
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
    let allocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let initialized = allocations.clone();
    let worker = thread::spawn(move || {
        let mut session = None;
        let mut native_state: Option<Value> = None;
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
            let (status, body) = if let Some(session_first) = session_first
                && request
                    .path
                    .starts_with("/api/rpc/openchamber.native-agent.provider.v1/")
            {
                let input: Value = serde_json::from_slice::<Value>(&request.body)
                    .expect("request body")["input"]
                    .clone();
                let draft = input["scope"]["sessionID"].is_null();
                let method = request.path.rsplit('/').next().expect("method");
                let mut state = if draft {
                    session_first_state(input["scope"].clone(), true, session_first)
                } else {
                    native_state.take().unwrap_or_else(|| {
                        session_first_state(input["scope"].clone(), false, false)
                    })
                };
                if !draft {
                    let created = &session
                        .as_ref()
                        .expect("real session precedes native execution")["data"];
                    assert_eq!(input["scope"]["sessionID"], created["id"]);
                    assert_eq!(
                        input["scope"]["directory"],
                        created["location"]["directory"]
                    );
                    assert_eq!(
                        created["agent"], "agent",
                        "provider policy has the captured agent's real session"
                    );
                    assert_eq!(created["model"]["id"], "model");
                }
                match method {
                    "getState" => {}
                    "setConfig" => {
                        assert!(!draft, "this first Send does not mutate draft settings");
                        assert_eq!(input["expectedRevision"], state["revision"]);
                        for change in input["changes"].as_array().expect("changes") {
                            let field = state["config"]
                                .as_array_mut()
                                .expect("test fixture")
                                .iter_mut()
                                .find(|field| field["id"] == change["id"])
                                .expect("only known fields before prepare; every field afterward");
                            assert!(
                                field["choices"]
                                    .as_array()
                                    .expect("test fixture")
                                    .iter()
                                    .any(|choice| choice["id"] == change["value"])
                            );
                            field["value"] = change["value"].clone();
                        }
                        state["revision"] =
                            json!(state["revision"].as_u64().expect("revision") + 1);
                        if state["status"] == "ready" {
                            state["execution"]["variant"] = json!("configured-binding");
                        }
                    }
                    "prepare" if draft => {
                        // Draft Prepare is metadata discovery, not SDK allocation.
                        assert_eq!(state["capabilities"]["prepareAfterSession"], true);
                        assert_eq!(state["status"], "unprepared");
                        assert_eq!(state["configComplete"], false);
                        assert!(state["execution"].is_null());
                        assert_eq!(state["requests"], json!([]));
                        assert_eq!(state["submission"]["status"], "none");
                    }
                    "prepare" => {
                        assert!(!draft, "allocation requires the policy-owning real session");
                        assert_eq!(
                            state["config"][0]["value"], "cloud",
                            "captured runtime intent is applied before allocation"
                        );
                        if initialized.load(Ordering::Acquire) == 0 {
                            let needs_auth =
                                authenticate && state["authentication"] != "authenticated";
                            state["authentication"] = json!("authenticated");
                            state["status"] = json!("ready");
                            state["revision"] =
                                json!(state["revision"].as_u64().expect("revision") + 1);
                            state["execution"] = json!({"providerID":"provider","modelID":"model","variant":"prepared-binding"});
                            state["configComplete"] = json!(true);
                            state["config"]
                                .as_array_mut()
                                .expect("test fixture")
                                .retain(|field| field["id"] == "runtime");
                            state["config"].as_array_mut().expect("config").extend([
                            json!({"id":"effort","label":"Effort","role":"effort","value":"normal","choices":[{"id":"normal","label":"Normal"},{"id":"deep","label":"Deep"}]}),
                            json!({"id":"new-field","label":"New field","role":"other","value":"server-default","choices":[{"id":"server-default","label":"Server default"}]}),
                        ]);
                            if needs_auth {
                                state["authentication"] = json!("required");
                                state["status"] = json!("unprepared");
                                state["execution"] = Value::Null;
                                state["configComplete"] = json!(false);
                            } else {
                                assert_eq!(
                                    initialized.fetch_add(1, Ordering::AcqRel),
                                    0,
                                    "one synthetic SDK initialization per real session"
                                );
                            }
                        } else {
                            // Prepare may refresh/reconcile an already prepared session.
                            // The fixture preserves its initialized SDK and current binding.
                            assert_eq!(state["status"], "ready");
                            assert_eq!(state["configComplete"], true);
                            assert!(!state["execution"].is_null());
                        }
                    }
                    "authenticate" => {
                        assert!(!draft);
                        if replace_agent_after_auth {
                            session.as_mut().expect("real session exists before login")["data"]["agent"] =
                                json!("external-replacement-agent");
                        }
                        assert_eq!(input["userInitiated"], true);
                        assert_eq!(input["expectedRevision"], state["revision"]);
                        state["authentication"] = json!("authenticated");
                        state["status"] = json!("unprepared");
                        state["execution"] = Value::Null;
                        state["configComplete"] = json!(false);
                        state["revision"] =
                            json!(state["revision"].as_u64().expect("revision") + 1);
                    }
                    other => panic!("unexpected native action {other}"),
                }
                if !draft {
                    native_state = Some(state.clone());
                }
                let output = if method == "prepare" && state["authentication"] == "required" {
                    json!({"protocolVersion":1,"requestID":input["requestID"],"ok":false,"error":{"code":"authentication_required","outcome":"rejected"},"state":state})
                } else {
                    json!({"protocolVersion":1,"requestID":input["requestID"],"ok":true,"state":state})
                };
                (200, json!({"output":output}))
            } else if session_first.is_some()
                && matches!(
                    request.path.as_str(),
                    "/api/provider" | "/api/model" | "/api/model/default"
                )
            {
                let data = if request.path == "/api/provider" {
                    json!([{"id":"provider","settings":{"openchamber":{"nativeAgent":{"protocolVersion":1}}}}])
                } else if request.path == "/api/model" {
                    json!([{"id":"model","modelID":"model","providerID":"provider","name":"Native route","enabled":true,"variants":[]}])
                } else {
                    Value::Null
                };
                (
                    200,
                    json!({"location":{"directory":"/project"},"data":data}),
                )
            } else if create {
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
            for gate in &holds {
                gate.wait(&request.method, &request.path);
            }
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
        allocations,
    )
}

/// Same five-second waiting budget as recovery_tests::wait. Print only changed
/// synthetic state, at most eight transitions per phase, plus the final failure.
fn wait_native_phase(
    cx: &mut TestAppContext,
    workspace: &gpui_kit::Entity<Workspace>,
    server: &Server,
    owner: Option<&openchamber_native_core::native_agent::OwnerKey>,
    phase: &str,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    let mut waited = Duration::ZERO;
    let mut previous = None;
    let mut printed = 0;
    loop {
        cx.run_until_parked();
        let ready = done(cx);
        let observation = cx.read(|cx| {
            let this = workspace.read(cx);
            let captured_owner = owner.cloned().or_else(|| this.creations.iter().find_map(|(key, attempt)| {
                let config = attempt.native_config.as_ref()?;
                Some(openchamber_native_core::native_agent::OwnerKey {
                    server: key.runtime.as_str().to_owned(),
                    scope: openchamber_native_core::native_agent::Scope {
                        directory: attempt.source.directory().to_owned(), provider_id: config.provider_id.clone(), session_id: Some(key.session.to_string()),
                    },
                })
            }));
            let owner = captured_owner.as_ref();
            let selection = this.catalog_selection();
            let state = owner.and_then(|key| this.native_agents.owner.state(key));
            let directory = this.config_directory();
            let catalog = directory.as_deref().and_then(|directory| this.send_catalog.directory(directory));
            let backend = server.requests.lock().expect("requests");
            let count = |suffix: &str| backend.iter().filter(|request| request.path.ends_with(suffix)).count();
            json!({
                "ready": ready,
                "runtime": this.runtime.as_str(),
                "selected": this.selected.as_ref().map(ToString::to_string),
                "composerDirectory": this.composer_key.as_ref().map(|key| key.directory()),
                "configDirectory": directory,
                "connectionGeneration": this.connection.as_ref().map(|connection| connection.generation),
                "connected": this.connection.as_ref().is_some_and(|connection| connection.state == crate::connection::ConnectionState::Connected),
                "capturedOwner": owner.map(|key| json!({"runtime":key.server,"directory":key.scope.directory,"provider":key.scope.provider_id,"session":key.scope.session_id})),
                "currentOwnerMatches": owner.is_some_and(|key| this.native_owner_key(&key.scope.provider_id).as_ref() == Some(key)),
                "catalogPresent": catalog.is_some(),
                "agentsReady": catalog.is_some_and(|entry| entry.agents.ready().is_some()),
                "providersReady": catalog.is_some_and(|entry| entry.providers.ready().is_some()),
                "routes": catalog.and_then(|entry| entry.providers.ready()).map(|providers| providers.iter().take(4).map(|provider| json!({"provider":provider.id,"models":provider.models.iter().take(4).map(|model| model.id.as_str()).collect::<Vec<_>>() })).collect::<Vec<_>>()),
                "catalogSelection": selection.map(|(directory, selection)| json!({"directory":directory,"agent":selection.agent_name,"model":selection.model.map(|model| format!("{}/{}", model.provider_id, model.model_id))})),
                "providerResolved": owner.is_some_and(|key| this.native_provider_advertised(&key.scope.provider_id)),
                "enterprise": this.enterprise.enterprise_mode(),
                "machinePolicyEnabled": this.enterprise.status.enabled,
                "machinePolicyUnavailable": this.enterprise.status.unavailable,
                "serverEnterpriseMode": this.enterprise.server.as_ref().map(|policy| policy.enterprise_mode),
                "status": state.map(|state| format!("{:?}", state.status)),
                "authentication": state.map(|state| format!("{:?}", state.authentication)),
                "activation": state.map(|state| state.activation_id.as_str()),
                "revision": state.map(|state| state.revision),
                "configComplete": state.map(|state| state.config_complete),
                "canAuthenticate": owner.is_some_and(|key| this.native_agents.owner.can_authenticate(key)),
                "requiresDiscovery": owner.is_some_and(|key| this.native_agents.owner.requires_discovery(key)),
                "nativeActionPending": owner.is_some_and(|key| this.native_action_pending(key)),
                "nativeFailure": owner.map(|key| format!("{:?}", this.native_agents.owner.failure(key))),
                "creationPhases": this.creations.iter().map(|(_, attempt)| format!("{:?}", attempt.phase)).collect::<Vec<_>>(),
                "correctionRequired": this.creations.iter().filter_map(|(_, attempt)| attempt.native_config.as_ref().map(|config| config.correction_required)).collect::<Vec<_>>(),
                "backend": {"total":backend.len(),"auth":count("/authenticate"),"prepare":count("/prepare"),"getState":count("/getState"),"setConfig":count("/setConfig"),"prompt":count("/prompt"),"modelReads":backend.iter().filter(|request| request.method == "GET" && request.path == "/api/model").count()},
                "recentNativeRPCs": backend.iter().rev().filter(|request| request.path.starts_with("/api/rpc/")).take(6).map(|request| {
                    let body: Value = serde_json::from_slice(&request.body).expect("request body");
                    json!({"method":request.path.rsplit('/').next(),"session":body["input"]["scope"]["sessionID"],"userInitiated":body["input"]["userInitiated"],"expectedRevision":body["input"]["expectedRevision"]})
                }).collect::<Vec<_>>()
            })
        });
        if previous.as_ref() != Some(&observation) {
            if printed < 8 {
                eprintln!("[native-fixture:{phase}] {observation}");
                printed += 1;
            }
            previous = Some(observation.clone());
        }
        if ready {
            return;
        }
        assert!(
            waited < Duration::from_secs(5),
            "native fixture phase {phase} timed out: {observation}"
        );
        let started = Instant::now();
        thread::sleep(Duration::from_millis(1));
        waited += started.elapsed();
    }
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

/// Retains the same mutation cache across workspaces. The strict synthetic
/// policy checks it for Prepare; Devin's policy exempts Prepare lookups.
/// Requests are recorded before lookup, including rejected collisions.
fn native_identity_server(
    prepare_exempt: bool,
) -> (
    Server,
    Arc<std::sync::atomic::AtomicUsize>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = Endpoint::parse(&format!(
        "http://{}",
        listener.local_addr().expect("address")
    ))
    .expect("endpoint");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let recorded = requests.clone();
    let stopped = stop.clone();
    let collisions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rejected = collisions.clone();
    let mutations_accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let accepted = mutations_accepted.clone();
    let worker = thread::spawn(move || {
        let scope = json!({"directory":"/project","providerID":"provider","sessionID":"ses_native_identity"});
        let mut state = session_first_state(scope.clone(), false, false);
        state["capabilities"]["prepareAfterSession"] = json!(false);
        state["status"] = json!("ready");
        state["authentication"] = json!("authenticated");
        state["execution"] =
            json!({"providerID":"provider","modelID":"model","variant":"initial-binding"});
        let mut mutations = std::collections::BTreeMap::<String, (Value, Value)>::new();
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
                .expect("test fixture");
            let request = read_request(&mut stream);
            let path = request.path.clone();
            let input: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            recorded.lock().expect("requests").push(request);
            let body = if path.starts_with("/api/rpc/") {
                let input = &input["input"];
                let method = path.rsplit('/').next().expect("method");
                assert_eq!(input["scope"], scope, "both workspaces use the same backend session");
                let request_id = input["requestID"].as_str().expect("request ID");
                let key = format!("ses_native_identity/{request_id}");
                let payload = json!({"method":method,"input":input});
                let prior = if prepare_exempt && method == "prepare" { None } else { mutations.get(&key) };
                let result = if let Some((prior, result)) = prior {
                    if *prior == payload { result.clone() } else {
                        rejected.fetch_add(1, Ordering::AcqRel);
                        json!({"protocolVersion":1,"requestID":request_id,"ok":false,"error":{"code":"invalid_request","outcome":"rejected"},"state":state})
                    }
                } else {
                    match method {
                        "prepare" => { state["revision"] = json!(state["revision"].as_u64().expect("revision") + 1); }
                        "getState" => {}
                        "setConfig" => {
                            assert_eq!(input["expectedRevision"], state["revision"]);
                            let value = input["changes"][0]["value"].as_str().expect("value");
                            assert!(matches!(value, "cloud" | "local"));
                            assert_eq!(input["changes"], json!([{"id":"runtime","value":value}]));
                            state["config"][0]["value"] = json!(value);
                            state["revision"] = json!(state["revision"].as_u64().expect("revision") + 1);
                            state["execution"]["variant"] = json!(format!("configured-{value}"));
                            accepted.fetch_add(1, Ordering::AcqRel);
                        }
                        other => panic!("unexpected identity regression RPC {other}"),
                    }
                    let result = json!({"protocolVersion":1,"requestID":request_id,"ok":true,"state":state});
                    if method == "setConfig" { mutations.insert(key, (payload, result.clone())); }
                    result
                };
                json!({"output":result})
            } else {
                let data = match path.as_str() {
                    "/api/provider" => json!([{"id":"provider","settings":{"openchamber":{"nativeAgent":{"protocolVersion":1}}}}]),
                    "/api/model" => json!([{"id":"model","modelID":"model","providerID":"provider","enabled":true,"variants":[]}]),
                    "/api/model/default" => Value::Null,
                    _ => json!([]),
                };
                json!({"location":{"directory":"/project"},"data":data})
            }.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (
        Server {
            endpoint,
            requests,
            stop,
            release_prompt: Arc::new(AtomicBool::new(true)),
            worker: Some(worker),
        },
        collisions,
        mutations_accepted,
    )
}

#[gpui_kit::test]
fn session_first_native_request_ids_survive_workspace_reopen_without_mutation_replay(
    cx: &mut TestAppContext,
) {
    native_request_identity_handoff(cx, false);
}

#[gpui_kit::test]
fn session_first_native_mutation_ids_survive_reopen_with_prepare_exempt_cache(
    cx: &mut TestAppContext,
) {
    native_request_identity_handoff(cx, true);
}

fn native_request_identity_handoff(cx: &mut TestAppContext, prepare_exempt: bool) {
    use openchamber_native_core::native_agent::{Call, ConfigChange, OwnerKey, Scope};
    let fixture = Fixture::new();
    let (server, collisions, mutations_accepted) = native_identity_server(prepare_exempt);
    let key = tests::key("native-identity-runtime", "ses_native_identity");
    let owner = OwnerKey {
        server: key.runtime.as_str().into(),
        scope: Scope {
            directory: "/project".into(),
            provider_id: "provider".into(),
            session_id: Some(key.session.to_string()),
        },
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test fixture");
    let client = ServerClient::new(server.endpoint.clone(), Credential::None).expect("client");
    let advertised = runtime
        .block_on(client.model_catalog("/project"))
        .expect("test fixture")
        .native_agents;
    for reopened in [false, true] {
        let (window, workspace) = open(cx, &server, &fixture, &key);
        ready(cx, &workspace);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                configure_send(this, &key, window, cx);
                let existing = this.send_catalog.directory("/project").expect("directory");
                let mut catalog = DirectoryCatalog::default();
                catalog.agents = Load::Ready(existing.agents.ready().expect("agents").clone());
                catalog.providers =
                    Load::Ready(existing.providers.ready().expect("providers").clone());
                catalog.opencode = Load::Ready(Default::default());
                catalog.native_agents = advertised.clone();
                this.send_catalog
                    .insert_test_directory("/project".into(), catalog);
                assert!(this.native_call(owner.clone(), Call::Prepare, true, cx));
            });
        })
        .expect("test fixture");
        wait_native_phase(
            cx,
            &workspace,
            &server,
            Some(&owner),
            "identity-discovery",
            |cx| {
                cx.read(|cx| {
                    let this = workspace.read(cx);
                    this.native_agents.owner.can_set_config(&owner)
                        && !this.native_action_pending(&owner)
                })
            },
        );
        if reopened {
            assert_eq!(
                mutations_accepted.load(Ordering::Acquire),
                1,
                "reopen discovery does not execute a cached mutation"
            );
            cx.read(|cx| {
                assert_eq!(
                    workspace
                        .read(cx)
                        .native_agents
                        .owner
                        .state(&owner)
                        .expect("test fixture")
                        .config[0]
                        .value,
                    "cloud"
                )
            });
            assert_eq!(
                server
                    .requests
                    .lock()
                    .expect("test fixture")
                    .iter()
                    .filter(|request| request.path.ends_with("/setConfig"))
                    .count(),
                1,
                "opening a new workspace does not replay the known mutation"
            );
        }
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |this, cx| {
                let call = if reopened && !prepare_exempt {
                    Call::Prepare
                } else {
                    Call::SetConfig(vec![ConfigChange {
                        id: "runtime".into(),
                        value: if reopened {
                            "local".into()
                        } else {
                            "cloud".into()
                        },
                    }])
                };
                let discover = matches!(call, Call::Prepare);
                assert!(this.native_call(owner.clone(), call, discover, cx));
            });
        })
        .expect("test fixture");
        wait_native_phase(
            cx,
            &workspace,
            &server,
            Some(&owner),
            "identity-action-complete",
            |cx| cx.read(|cx| !workspace.read(cx).native_action_pending(&owner)),
        );
        cx.read(|cx| {
            let this = workspace.read(cx);
            assert_eq!(
                collisions.load(Ordering::Acquire),
                0,
                "explicit reopened action must not reuse the cached mutation identity"
            );
            assert!(this.native_agents.owner.failure(&owner).is_none());
            assert_eq!(
                this.native_agents
                    .owner
                    .state(&owner)
                    .expect("state")
                    .config[0]
                    .value,
                if reopened && prepare_exempt {
                    "local"
                } else {
                    "cloud"
                }
            );
            assert_eq!(
                mutations_accepted.load(Ordering::Acquire),
                if reopened && prepare_exempt { 2 } else { 1 },
                "count accepted explicit mutations separately from discovery RPCs"
            );
        });
        close(cx, window, &workspace);
    }
    let recorded = server.requests.lock().expect("requests");
    let native: Vec<_> = recorded
        .iter()
        .filter(|request| request.path.starts_with("/api/rpc/"))
        .collect();
    assert_eq!(
        native.len(),
        4,
        "two fresh workspaces each issue discovery and one explicit action"
    );
    let ids: Vec<_> = native
        .iter()
        .map(|request| {
            serde_json::from_slice::<Value>(&request.body).expect("request body")["input"]["requestID"]
                .as_str()
                .expect("test fixture")
                .to_owned()
        })
        .collect();
    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        ids.len()
    );
    assert!(
        ids.iter()
            .all(|id| openchamber_native_core::native_agent::valid_id(id))
    );
    assert_eq!(
        recorded
            .iter()
            .filter(|request| request.path.ends_with("/setConfig"))
            .count(),
        if prepare_exempt { 2 } else { 1 }
    );
    assert_eq!(
        recorded
            .iter()
            .filter(|request| request.path.ends_with("/prompt"))
            .count(),
        0
    );
}

fn session_first_handoff(
    cx: &mut TestAppContext,
    invalid: bool,
    authenticate: bool,
    restart_pending: bool,
    replace_agent_after_auth: bool,
) {
    use openchamber_native_core::draft_content::{DraftFile, DraftFileContent};
    use openchamber_native_core::ids::AttachmentId;
    use openchamber_native_core::native_agent::{Call, OwnerKey, Scope};
    let fixture = Fixture::new();
    let key = tests::key("session-first-runtime", "ses_existing");
    let (server, _, allocations) = creation_server_with_native(
        fixture.journal(),
        Outcome::Success,
        true,
        Vec::new(),
        Some(invalid),
        authenticate,
        replace_agent_after_auth,
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test fixture");
    let client = ServerClient::new(server.endpoint.clone(), Credential::None).expect("client");
    let advertised = runtime
        .block_on(client.model_catalog("/project"))
        .expect("test fixture")
        .native_agents;
    assert_eq!(advertised.len(), 1);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    if authenticate {
        // Production main starts this monitor after Workspace::new. The headless
        // opener leaves machine policy unread and therefore fail-closed.
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |this, cx| this.start_policy_monitor(cx));
        })
        .expect("test fixture");
        wait_native_phase(
            cx,
            &workspace,
            &server,
            None,
            "machine-policy-settled",
            |cx| cx.read(|cx| !workspace.read(cx).enterprise.status.unavailable),
        );
        cx.read(|cx| {
            assert!(!workspace.read(cx).enterprise.enterprise_mode(), "authentication success fixture requires an authoritatively non-enterprise host; enabled enterprise policy must continue to refuse authentication");
        });
    }
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            prepare(this, &key, window, cx);
            let existing = this.send_catalog.directory("/project").expect("directory");
            let mut catalog = DirectoryCatalog::default();
            catalog.agents = Load::Ready(existing.agents.ready().expect("agents").clone());
            catalog.providers = Load::Ready(existing.providers.ready().expect("providers").clone());
            catalog.opencode = Load::Ready(Default::default());
            catalog.skills = Load::Ready(0);
            catalog.mcp_connected = Load::Ready(0);
            catalog.native_agents = advertised.clone();
            this.send_catalog
                .insert_test_directory("/project".into(), catalog);
            let source = this.composer_key.clone().expect("composer key");
            let owner = OwnerKey {
                server: this.runtime.as_str().into(),
                scope: Scope {
                    directory: source.directory().into(),
                    provider_id: "provider".into(),
                    session_id: None,
                },
            };
            let api = openchamber_native_transport::native_agent::NativeAgentApi::new(
                client.clone(),
                owner.clone(),
                &advertised[0],
            )
            .expect("test fixture");
            let ticket = this
                .native_agents
                .owner
                .begin(owner.clone(), "draft-metadata".into(), Call::GetState, true)
                .expect("test fixture");
            let reply = runtime.block_on(api.call(&ticket)).expect("reply");
            assert!(this.native_agents.owner.complete(&ticket, reply));
            assert!(!this.native_agents.owner.can_submit(&owner));
            let mut draft = this.composer_draft(cx);
            draft.content.files.push(DraftFile {
                id: AttachmentId::from("captured-reference"),
                name: Some("kept.txt".into()),
                media_type: Some("text/plain".into()),
                byte_len: None,
                source_document_id: None,
                content: DraftFileContent::Reference {
                    uri: "file:///project/kept.txt".into(),
                },
            });
            this.drafts.save(source, draft);
            this.restore_composer_draft(window, cx);
            assert!(
                this.can_create_connected(),
                "config-only metadata permits the accepted first Send"
            );
            this.submit(window, cx);
        });
    })
    .expect("window");
    wait_native_phase(cx, &workspace, &server, None, "creation-recorded", |_| {
        create_count(&server) == 1
    });
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.composer.update(cx, |composer, cx| {
                composer.set_value("later independent draft", window, cx)
            })
        });
    })
    .expect("window");
    server.release_prompt.store(true, Ordering::Release);
    if authenticate {
        let owner = cx.read(|cx| {
            let this = workspace.read(cx);
            let (key, attempt) = this
                .creations
                .iter()
                .find(|(_, attempt)| {
                    attempt
                        .native_config
                        .as_ref()
                        .is_some_and(|config| config.prepare_after_session)
                })
                .expect("captured pending native creation");
            OwnerKey {
                server: key.runtime.as_str().to_owned(),
                scope: Scope {
                    directory: attempt.source.directory().to_owned(),
                    provider_id: attempt
                        .native_config
                        .as_ref()
                        .expect("native config")
                        .provider_id
                        .clone(),
                    session_id: Some(key.session.to_string()),
                },
            }
        });
        wait_native_phase(
            cx,
            &workspace,
            &server,
            Some(&owner),
            "authentication-required",
            |cx| {
                cx.read(|cx| {
                    let this = workspace.read(cx);
                    this.native_agents.owner.state(&owner).is_some_and(|state| {
                        state.authentication
                            == openchamber_native_core::native_agent::Authentication::Required
                    })
                })
            },
        );
        assert_eq!(server.prompt_count(), 0);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.select_session(
                    openchamber_native_core::ids::SessionId::from(
                        owner.scope.session_id.as_deref().expect("session ID"),
                    ),
                    window,
                    cx,
                );
            });
        })
        .expect("test fixture");
        // Match the visible sign-in control's scope and enabled predicate after
        // navigation/discovery callbacks complete. Retained state alone is not authority.
        wait_native_phase(
            cx,
            &workspace,
            &server,
            Some(&owner),
            "sign-in-enabled-after-selection",
            |cx| {
                cx.read(|cx| {
                    let this = workspace.read(cx);
                    this.native_owner_key(&owner.scope.provider_id).as_ref() == Some(&owner)
                        && this
                            .catalog_selection()
                            .and_then(|(_, selection)| selection.model)
                            .is_some_and(|model| {
                                model.provider_id == owner.scope.provider_id
                                    && this.native_provider_advertised(&model.provider_id)
                            })
                        && !this.enterprise.enterprise_mode()
                        && this.native_agents.owner.state(&owner).is_some_and(|state| {
                            state.authentication
                                == openchamber_native_core::native_agent::Authentication::Required
                        })
                        && this.native_agents.owner.can_authenticate(&owner)
                        && !this.native_action_pending(&owner)
                })
            },
        );
        cx.update_window(window, |_, _, cx| {
            workspace.update(cx, |this, cx| {
                assert!(
                    this.native_call(owner.clone(), Call::Authenticate, false, cx),
                    "enabled sign-in control admits authentication for the captured pending owner"
                );
            });
        })
        .expect("test fixture");
        wait_native_phase(
            cx,
            &workspace,
            &server,
            Some(&owner),
            "authentication-recorded",
            |_| {
                server
                    .requests
                    .lock()
                    .expect("test fixture")
                    .iter()
                    .any(|request| request.path.ends_with("/authenticate"))
            },
        );
        assert_eq!(
            server
                .requests
                .lock()
                .expect("test fixture")
                .iter()
                .filter(|request| request.path.ends_with("/authenticate"))
                .count(),
            1,
            "one authentication request reached the backend; the replacement-agent scenario mutates its real session in this callback"
        );
    }
    if replace_agent_after_auth {
        wait_native_phase(
            cx,
            &workspace,
            &server,
            None,
            "external-agent-reconciliation",
            |cx| {
                server.prompt_count() > 0
                    || cx.read(|cx| {
                        let this = workspace.read(cx);
                        this.creations
                            .checks
                            .values()
                            .any(|check| *check == CreationCheck::Mismatch)
                    })
            },
        );
        assert_eq!(
            server.prompt_count(),
            0,
            "authentication continuation must freshly verify the real agent before prompt admission"
        );
        cx.read(|cx| {
            let this = workspace.read(cx);
            let (_, pending) = this
                .creations
                .iter()
                .next()
                .expect("mismatched session keeps accepted content pending");
            assert_eq!(pending.request.agent(), Some("agent"));
            assert_eq!(pending.prompt.agent.as_deref(), Some("agent"));
            assert!(!pending.prompt.text.is_empty());
        });
        assert_eq!(create_count(&server), 1);
        close(cx, window, &workspace);
        return;
    }
    if invalid {
        wait_native_phase(
            cx,
            &workspace,
            &server,
            None,
            "invalid-config-correction-required",
            |cx| {
                cx.read(|cx| {
                    workspace.read(cx).creations.iter().any(|(_, attempt)| {
                        attempt
                            .native_config
                            .as_ref()
                            .is_some_and(|config| config.correction_required)
                    })
                })
            },
        );
        assert_eq!(
            server.prompt_count(),
            0,
            "invalid captured intent does not dispatch or silently accept defaults"
        );
        let pending = cx.read(|cx| {
            workspace
                .read(cx)
                .creations
                .iter()
                .next()
                .expect("test fixture")
                .0
                .clone()
        });
        if restart_pending {
            let frozen = cx.read(|cx| {
                workspace
                    .read(cx)
                    .creations
                    .get(&pending)
                    .expect("creation")
                    .clone()
            });
            close(cx, window, &workspace);
            let native_calls = server
                .requests
                .lock()
                .expect("test fixture")
                .iter()
                .filter(|request| request.path.starts_with("/api/rpc/"))
                .count();
            let (window, reopened) = open(cx, &server, &fixture, &key);
            ready(cx, &reopened);
            cx.read(|cx| {
                let held = reopened.read(cx).creations.get(&pending).expect("creation");
                assert_eq!(held.request, frozen.request);
                assert_eq!(held.native_config, frozen.native_config);
                assert_eq!(held.prompt.message_id, frozen.prompt.message_id);
                assert_eq!(held.prompt.text, frozen.prompt.text);
                assert_eq!(held.prompt.parts.files, frozen.prompt.parts.files);
                assert!(held.navigation.is_none());
            });
            assert_eq!(create_count(&server), 1);
            assert_eq!(server.prompt_count(), 0);
            assert_eq!(
                server
                    .requests
                    .lock()
                    .expect("test fixture")
                    .iter()
                    .filter(|request| request.path.starts_with("/api/rpc/"))
                    .count(),
                native_calls
            );
            close(cx, window, &reopened);
            return;
        }
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.select_session(pending.session.clone(), window, cx);
                this.check_creation(&pending, true, window, cx)
            });
        })
        .expect("test fixture");
    }
    wait_native_phase(
        cx,
        &workspace,
        &server,
        None,
        "first-prompt-recorded",
        |_| server.prompt_count() == 1,
    );
    wait_native_phase(
        cx,
        &workspace,
        &server,
        None,
        "prompt-mutation-complete",
        |cx| cx.read(|cx| workspace.read(cx).mutation_tasks.is_empty()),
    );
    {
        let recorded = server.requests.lock().expect("requests");
        let rpc_ids: Vec<_> = recorded
            .iter()
            .filter(|request| request.path.starts_with("/api/rpc/"))
            .map(|request| {
                serde_json::from_slice::<Value>(&request.body).expect("request body")["input"]["requestID"]
                    .as_str()
                    .expect("test fixture")
                    .to_owned()
            })
            .collect();
        assert!(
            rpc_ids.iter().any(|id| id.starts_with("native-create-")),
            "first Send exercises creation-path request identities"
        );
        assert!(
            rpc_ids
                .iter()
                .all(|id| openchamber_native_core::native_agent::valid_id(id))
        );
        assert_eq!(
            rpc_ids
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            rpc_ids.len(),
            "each newly issued creation or ordinary RPC has a distinct valid correlation ID"
        );
        let creation = recorded
            .iter()
            .find(|request| request.method == "POST" && request.path == "/api/session")
            .expect("test fixture");
        let body: Value = serde_json::from_slice(&creation.body).expect("request body");
        assert_eq!(body["agent"], "agent");
        assert_eq!(body["model"]["providerID"], "provider");
        assert_eq!(body["model"]["id"], "model");
        let captured = &creation
            .journal
            .as_ref()
            .expect("capture is durable before creation")["creations"][0];
        assert_eq!(captured["request_agent"], "agent");
        assert_eq!(captured["native_config"]["prepare_after_session"], true);
        assert!(
            captured["native_config"]["variant"].is_null(),
            "no draft binding is invented"
        );
        let create_index = recorded
            .iter()
            .position(|request| request.path == "/api/session" && request.method == "POST")
            .expect("test fixture");
        let prepares: Vec<_> = recorded
            .iter()
            .enumerate()
            .filter(|(_, request)| request.path.ends_with("/prepare"))
            .map(|(index, request)| {
                let body: Value = serde_json::from_slice(&request.body).expect("request body");
                (index, body["input"]["scope"]["sessionID"].clone())
            })
            .collect();
        let draft_prepares = prepares
            .iter()
            .filter(|(_, session)| session.is_null())
            .count();
        let real_prepares = prepares
            .iter()
            .filter(|(_, session)| !session.is_null())
            .count();
        assert!(
            draft_prepares > 0,
            "config-only draft Prepare discovery is allowed"
        );
        eprintln!(
            "[native-fixture:handoff-counts] draftPrepareRPCs={draft_prepares} realPrepareRPCs={real_prepares} syntheticSDKInitializations={} prompts={}",
            allocations.load(Ordering::Acquire),
            recorded
                .iter()
                .filter(|request| request.path.ends_with("/prompt"))
                .count()
        );
        assert_eq!(
            allocations.load(Ordering::Acquire),
            1,
            "discovery and explicit recovery Prepare RPCs reuse one synthetic SDK initialization"
        );
        let prepare_index = prepares
            .iter()
            .find(|(_, session)| *session == body["id"])
            .map(|(index, _)| *index)
            .expect("test fixture");
        assert!(create_index < prepare_index);
        let preconfig = recorded[..prepare_index]
            .iter()
            .find(|request| request.path.ends_with("/setConfig"))
            .expect("test fixture");
        let input: Value = serde_json::from_slice(&preconfig.body).expect("request body");
        assert_eq!(
            input["input"]["changes"],
            json!([{"id":"runtime","value":"cloud"}])
        );
        assert_eq!(input["input"]["scope"]["sessionID"], body["id"]);
        let prompt = recorded
            .iter()
            .find(|request| request.path.ends_with("/prompt"))
            .expect("test fixture");
        let prompt_body: Value = serde_json::from_slice(&prompt.body).expect("request body");
        assert_eq!(prompt_body["text"], "first creation prompt");
        assert_eq!(prompt_body["files"][0]["uri"], "file:///project/kept.txt");
        let last_config = recorded
            .iter()
            .rev()
            .find(|request| request.path.ends_with("/setConfig"))
            .expect("test fixture");
        let input: Value = serde_json::from_slice(&last_config.body).expect("request body");
        assert!(
            input["input"]["changes"]
                .as_array()
                .expect("test fixture")
                .iter()
                .any(|change| change["id"] == "effort"
                    && change["value"] == if invalid { "normal" } else { "deep" })
        );
        let model = recorded
            .iter()
            .rev()
            .find(|request| request.path.ends_with("/model") && request.method == "POST")
            .expect("test fixture");
        let model_body: Value = serde_json::from_slice(&model.body).expect("request body");
        assert_eq!(model_body["model"]["variant"], "configured-binding");
    }
    close(cx, window, &workspace);
    let (window, reopened) = open(cx, &server, &fixture, &key);
    ready(cx, &reopened);
    assert_eq!(create_count(&server), 1);
    assert_eq!(
        server.prompt_count(),
        1,
        "restoring an unknown admission never replays it"
    );
    close(cx, window, &reopened);
}

#[gpui_kit::test]
fn session_first_send_captures_before_create_preconfigures_then_binds_live_execution(
    cx: &mut TestAppContext,
) {
    session_first_handoff(cx, false, false, false, false);
}

#[gpui_kit::test]
fn session_first_invalid_live_config_retains_message_until_explicit_durable_correction(
    cx: &mut TestAppContext,
) {
    session_first_handoff(cx, true, false, false, false);
}

#[gpui_kit::test]
fn session_first_authentication_reply_resumes_the_accepted_message_once_without_an_event(
    cx: &mut TestAppContext,
) {
    session_first_handoff(cx, false, true, false, false);
}

#[gpui_kit::test]
fn session_first_restart_retains_undispatched_capture_without_preparation_or_prompt_replay(
    cx: &mut TestAppContext,
) {
    session_first_handoff(cx, true, false, true, false);
}

#[gpui_kit::test]
fn session_first_authentication_resume_reconciles_external_agent_change_without_dispatch(
    cx: &mut TestAppContext,
) {
    session_first_handoff(cx, false, true, false, true);
}

#[gpui_kit::test]
fn creation_handoff_does_not_cancel_global_form_arriving_after_activation(cx: &mut TestAppContext) {
    use openchamber_native_core::request_owner::RequestOwner;
    let fixture = Fixture::new();
    let key = tests::key("create-activation", "ses_existing");
    let (server, _) = creation_server(fixture.journal(), Outcome::Success, true);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            prepare(this, &key, window, cx);
            this.submit(window, cx);
        })
    })
    .expect("window");
    wait(cx, |_| create_count(&server) == 1);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            super::request_tests::ask(
                this,
                super::request_tests::form(RequestOwner::Location),
                window,
                cx,
            );
        })
    })
    .expect("window");
    server.release_prompt.store(true, Ordering::Release);
    wait(cx, |cx| {
        cx.read(|cx| {
            let this = workspace.read(cx);
            this.creations.iter().next().is_none()
                && this.creation_tasks.is_empty()
                && this.mutation_tasks.is_empty()
        })
    });
    assert!(
        server
            .requests
            .lock()
            .expect("requests")
            .iter()
            .all(|request| !request.path.contains("/form/")),
        "handoff cannot dismiss a later form"
    );
    assert_eq!(
        server.prompt_count(),
        0,
        "late global form blocks the bound prompt"
    );
    cx.read(|cx| {
        let this = workspace.read(cx);
        let (_, attempt) = this
            .send_attempts
            .iter()
            .next()
            .expect("atomic handoff retains prompt");
        assert_eq!(attempt.origin, SendOrigin::Created);
        assert_eq!(attempt.prompt.text, "first creation prompt");
        assert_eq!(attempt.admission, Admission::Uncertain);
        assert_eq!(this.visible_requests().len(), 1);
    });
    close(cx, window, &workspace);
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
fn a_drafts_permission_choice_is_written_onto_its_session_beside_the_first_send(
    cx: &mut TestAppContext,
) {
    use openchamber_native_core::permission_mode::PermissionMode;
    let fixture = Fixture::new();
    let key = tests::key("create-runtime", "ses_existing");
    // The fixture answers the mode write with 404: its failure must not hold
    // back the prompt.
    let (server, _) = creation_server(fixture.journal(), Outcome::Success, false);
    let (window, workspace) = open(cx, &server, &fixture, &key);
    ready(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            prepare(workspace, &key, window, cx);
            workspace.cycle_permission_mode(window, cx);
            assert_eq!(
                workspace.composer_permission_mode(),
                PermissionMode::Auto,
                "without a safety net the draft goes straight to Auto"
            );
            workspace.submit(window, cx);
            assert_eq!(
                workspace.composer_permission_mode(),
                PermissionMode::Ask,
                "the next draft starts from the default again"
            );
        })
    })
    .expect("window");
    // The prompt's own outcome is not this test's subject: the fixture's
    // malformed answer leaves it uncertain and starts recovery reads.
    wait(cx, |cx| {
        server.prompt_count() == 1
            && cx.read(|cx| workspace.read(cx).permission_modes.writes_in_flight() == 0)
    });
    let requests = server.requests.lock().expect("requests");
    let create = requests
        .iter()
        .find(|request| request.path == "/api/session")
        .expect("create");
    let id = serde_json::from_slice::<Value>(&create.body).expect("body")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let writes: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "PUT")
        .collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].path,
        format!("/api/permission-auto-accept/sessions/{id}")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&writes[0].body).expect("write body"),
        json!({"mode": "auto", "enabled": true, "directory": "/project"})
    );
    let position = |path: &str| {
        requests
            .iter()
            .position(|request| request.path == path)
            .expect("request")
    };
    assert!(
        position("/api/session") < position(&writes[0].path),
        "the mode is written once the session exists"
    );
    drop(requests);
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
            workspace.check_recovery(
                &captured,
                &workspace.recovery_message(&captured, cx),
                window,
                cx,
            )
        })
    })
    .expect("window");
    wait(cx, |cx| {
        cx.read(|cx| workspace.read(cx).creation_tasks.is_empty())
    });
    assert_eq!(server.prompt_count(), 0);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.retry_recovery(
                &captured,
                &workspace.recovery_message(&captured, cx),
                window,
                cx,
            )
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
fn lost_creation_callback_restores_unknown_identity_without_resubmitting(cx: &mut TestAppContext) {
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
    // The server accepted the create and still holds its reply. Explicitly lose
    // the foreground callback before normal close drains the remaining work.
    cx.update(|cx| {
        workspace.update(cx, |workspace, _| {
            drop(
                workspace
                    .creation_tasks
                    .remove(&captured)
                    .expect("held creation owns its foreground callback"),
            );
        });
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
            workspace.check_recovery(
                &captured,
                &workspace.recovery_message(&captured, cx),
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
                workspace.retry_recovery(
                    &captured,
                    &workspace.recovery_message(&captured, cx),
                    window,
                    cx,
                )
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
