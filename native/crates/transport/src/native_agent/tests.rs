use super::*;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../../../packages/sdk/fixtures/native-agent-v1.json"
    ))
    .expect("SDK fixture")
}

#[test]
fn session_first_capability_is_optional_boolean_and_draft_execution_is_rejected() {
    let draft = fixture()["sessionFirstDraft"].clone();
    assert!(
        parse_state(&draft)
            .expect("SDK state")
            .capabilities
            .prepare_after_session
    );
    for flag in [None, Some(json!(false)), Some(json!(true))] {
        let mut value = draft.clone();
        if let Some(flag) = flag {
            value["capabilities"]["prepareAfterSession"] = flag;
        } else {
            value["capabilities"]
                .as_object_mut()
                .expect("capabilities object")
                .remove("prepareAfterSession");
        }
        assert!(parse_state(&value).is_ok());
    }
    for flag in [Value::Null, json!(0), json!("true"), json!({})] {
        let mut value = draft.clone();
        value["capabilities"]["prepareAfterSession"] = flag;
        assert!(parse_state(&value).is_err());
    }
    let mut invalid = draft;
    invalid["status"] = json!("ready");
    assert!(parse_state(&invalid).is_err());
    invalid["scope"]["sessionID"] = json!("real-session");
    assert!(parse_state(&invalid).is_ok());
}

#[test]
fn session_first_config_completeness_is_explicit_and_binding_requires_it() {
    let mut state = fixture()["sessionFirstDraft"].clone();
    state["scope"]["sessionID"] = json!("real-session");
    state["status"] = json!("ready");
    state["authentication"] = json!("authenticated");
    for complete in [None, Some(json!(false)), Some(json!(true))] {
        let mut current = state.clone();
        if let Some(value) = &complete {
            current["configComplete"] = value.clone();
        } else {
            current
                .as_object_mut()
                .expect("state object")
                .remove("configComplete");
        }
        assert_eq!(
            parse_state(&current).expect("SDK state").config_complete,
            complete == Some(json!(true))
        );
        current["execution"] =
            json!({"providerID":"session-first-native","modelID":"route","variant":"immutable"});
        assert_eq!(parse_state(&current).is_ok(), complete == Some(json!(true)));
    }
    for complete in [Value::Null, json!(0), json!("true"), json!({})] {
        state["configComplete"] = complete;
        assert!(parse_state(&state).is_err());
    }
}
fn state(index: usize) -> State {
    parse_state(&fixture()["providers"][index]["state"]).expect("SDK state")
}
fn key(state: &State) -> OwnerKey {
    OwnerKey {
        server: "verified-server".into(),
        scope: state.scope.clone(),
    }
}
fn adopt(owner: &mut NativeAgents, state: State, request: &str) -> OwnerKey {
    let key = key(&state);
    let ticket = owner
        .begin(key.clone(), request.into(), Call::GetState, true)
        .expect("discover");
    assert!(owner.complete(
        &ticket,
        Reply::Success {
            request_id: request.into(),
            state
        }
    ));
    key
}

#[test]
fn native_agent_sdk_fixture_parity_and_contextual_validation() {
    let fixture = fixture();
    for (index, provider) in fixture["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .enumerate()
    {
        let state = state(index);
        let settings = json!({"openchamber":{"nativeAgent":provider["discovery"]}});
        assert!(discovery(&state.scope.provider_id, Some(&settings)).is_some());
        for field in &state.config {
            assert!(validate_config(
                &state.config,
                &[ConfigChange {
                    id: field.id.clone(),
                    value: field.value.clone()
                }]
            ));
        }
    }
    let synthetic = state(1);
    for (request, value) in synthetic
        .requests
        .iter()
        .zip(fixture["answers"].as_array().expect("answers"))
    {
        let answer = if value["kind"] == "permission" {
            Answer::Permission {
                request_id: value["requestID"].as_str().expect("request ID").into(),
                choice_id: value["choiceID"].as_str().expect("choice ID").into(),
            }
        } else {
            Answer::Question {
                request_id: value["requestID"].as_str().expect("request ID").into(),
                values: value["values"]
                    .as_array()
                    .expect("values")
                    .iter()
                    .map(|v| QuestionValue {
                        field_id: v["fieldID"].as_str().expect("field ID").into(),
                        choice_ids: v["choiceIDs"]
                            .as_array()
                            .expect("choice IDs")
                            .iter()
                            .map(|s| s.as_str().expect("choice ID").into())
                            .collect(),
                        text: v.get("text").and_then(Value::as_str).map(str::to_owned),
                    })
                    .collect(),
            }
        };
        assert!(validate_answer(request, &answer));
        assert_eq!(answer_json(&answer), *value);
    }
    let AgentEvent::ToolProgress(progress) =
        parse_event(&fixture["progress"]).expect("SDK progress")
    else {
        panic!("progress");
    };
    assert_eq!(progress.scope, synthetic.scope);
    assert_eq!(progress.tool.call_id, "call-fixture");
    assert!(matches!(
        parse_reply(&fixture["failure"]).expect("SDK failure"),
        Reply::Failure {
            code: ErrorCode::OutcomeUnknown,
            outcome: Outcome::Unknown,
            state: None,
            ..
        }
    ));
    for (method, index, call) in [
        ("getState", 1, Call::GetState),
        ("prepare", 0, Call::Prepare),
        ("authenticate", 0, Call::Authenticate),
        (
            "setConfig",
            0,
            Call::SetConfig(vec![ConfigChange {
                id: "runtime-model".into(),
                value: "opaque-model-b".into(),
            }]),
        ),
        (
            "answer",
            1,
            Call::Answer(Answer::Permission {
                request_id: "permission-fixture".into(),
                choice_id: "yes-once".into(),
            }),
        ),
    ] {
        let mut owner = NativeAgents::default();
        let mut owner_state = state(index);
        // Reads discover with null activation as in the shared examples.
        let key = if call.is_mutation() {
            adopt(&mut owner, owner_state.clone(), &format!("seed-{method}"))
        } else {
            key(&owner_state)
        };
        let expected = &fixture["parameters"][method];
        let ticket = owner
            .begin(
                key,
                expected["requestID"].as_str().expect("request ID").into(),
                call,
                !expected["activationID"].is_string(),
            )
            .expect("begin");
        assert_eq!(parameters(&ticket).expect("parameters"), *expected);
        owner_state.revision += 1;
        owner.complete(
            &ticket,
            Reply::Success {
                request_id: ticket.request_id.clone(),
                state: owner_state,
            },
        );
    }
}

#[test]
fn native_agent_marker_is_explicit_strict_and_versioned() {
    for settings in [
        Value::Null,
        json!({}),
        json!({"nativeAgent":{"protocolVersion":1}}),
        json!({"openchamber":{"nativeAgent":{"protocolVersion":2}}}),
        json!({"openchamber":{"nativeAgent":{"protocolVersion":1,"rpc":"evil"}}}),
    ] {
        assert!(discovery("devin", Some(&settings)).is_none());
    }
    assert!(discovery("plain", None).is_none());
}

#[test]
fn native_agent_strict_bounds_reject_malformed_states_without_content_in_errors() {
    let original = fixture()["providers"][1]["state"].clone();
    for pointer in [
        "/protocolVersion",
        "/revision",
        "/status",
        "/authentication",
        "/permissionOwner",
        "/capabilities/answer",
        "/config/0/value",
        "/requests/0/choices/0/decision",
        "/submission/status",
        "/diagnostic/code",
    ] {
        let mut bad = original.clone();
        *bad.pointer_mut(pointer).expect("fixture field") = json!("sensitive-invalid-value");
        let error = parse_state(&bad).expect_err("invalid state");
        assert!(!format!("{error:?}").contains("sensitive-invalid-value"));
    }
    for (pointer, value) in [
        ("/revision", json!(MAX_SAFE_INTEGER + 1)),
        ("/revision", json!(-1)),
        ("/revision", json!(1.5)),
        ("/permissionOwner", json!("opencode")),
        ("/scope/directory", json!("relative")),
        ("/activationID", json!("x".repeat(257))),
        ("/activationID", json!("a\nb")),
        ("/requests/0/description", json!("a".repeat(16_385))),
    ] {
        let mut bad = original.clone();
        *bad.pointer_mut(pointer).expect("fixture field") = value;
        assert!(parse_state(&bad).is_err(), "{pointer}");
    }
    let mut extra = original.clone();
    extra["secret"] = json!("private");
    assert!(parse_state(&extra).is_err());
    let mut duplicate = original.clone();
    duplicate["requests"]
        .as_array_mut()
        .expect("fixture object")
        .push(original["requests"][0].clone());
    assert!(parse_state(&duplicate).is_err());
    let mut float = original;
    float["revision"] = json!(12.0);
    assert_eq!(parse_state(&float).expect("SDK state").revision, 12);
}

#[test]
fn native_agent_reconciliation_retains_owners_and_rejects_stale_generations_and_activations() {
    let initial = state(1);
    let mut owner = NativeAgents::default();
    assert!(
        !owner.state_changed("verified-server", initial.clone()),
        "events cannot establish activation"
    );
    let key = adopt(&mut owner, initial.clone(), "discover");
    let old = owner
        .begin(key.clone(), "late".into(), Call::GetState, false)
        .expect("begin");
    let mut newer = initial.clone();
    newer.revision += 1;
    assert!(owner.state_changed(&key.server, newer.clone()));
    assert!(!owner.state_changed(&key.server, initial.clone()));
    let mut replacement = initial.clone();
    replacement.activation_id = "replacement".into();
    replacement.revision = 0;
    assert!(!owner.state_changed(&key.server, replacement.clone()));
    assert!(owner.complete(
        &old,
        Reply::Success {
            request_id: "late".into(),
            state: initial.clone()
        }
    ));
    assert_eq!(owner.state(&key), Some(&newer));
    let late = owner
        .begin(key.clone(), "pre-switch".into(), Call::Prepare, true)
        .expect("begin");
    owner.invalidate();
    assert!(!owner.complete(
        &late,
        Reply::Success {
            request_id: "pre-switch".into(),
            state: replacement.clone()
        }
    ));
    assert_eq!(owner.state(&key), Some(&newer));
    let mut other = state(0);
    other.scope.directory = key.scope.directory.clone();
    other.scope.session_id = key.scope.session_id.clone();
    adopt(&mut owner, other, "other-provider");
    assert_eq!(
        owner.state(&key).expect("state").requests.len(),
        2,
        "next-message provider keeps old approvals"
    );
    let discover = owner
        .begin(key.clone(), "replacement-read".into(), Call::GetState, true)
        .expect("begin");
    owner.complete(
        &discover,
        Reply::Success {
            request_id: "replacement-read".into(),
            state: replacement.clone(),
        },
    );
    assert_eq!(owner.state(&key), Some(&replacement));
    let read = owner
        .begin(key.clone(), "failed-read".into(), Call::GetState, false)
        .expect("begin");
    owner.failed(&read);
    assert_eq!(owner.state(&key), Some(&replacement));
}

#[test]
fn native_agent_stale_discovery_cannot_reauthorize_retained_state_after_invalidation() {
    let initial = state(1);
    let mut owner = NativeAgents::default();
    let key = adopt(&mut owner, initial.clone(), "discover");
    owner.invalidate();
    let mut older = initial.clone();
    older.revision -= 1;
    let read = owner
        .begin(key.clone(), "stale-discovery".into(), Call::GetState, true)
        .expect("begin");
    owner.complete(
        &read,
        Reply::Success {
            request_id: read.request_id.clone(),
            state: older,
        },
    );
    assert_eq!(owner.state(&key), Some(&initial));
    assert!(matches!(
        owner.begin(
            key.clone(),
            "cannot-mutate".into(),
            Call::Authenticate,
            false
        ),
        Err(ErrorCode::NotPrepared)
    ));
    let read = owner
        .begin(key.clone(), "equal-discovery".into(), Call::GetState, true)
        .expect("begin");
    owner.complete(
        &read,
        Reply::Success {
            request_id: read.request_id.clone(),
            state: initial,
        },
    );
    assert!(
        owner
            .begin(
                key,
                "fresh-config".into(),
                Call::SetConfig(vec![ConfigChange {
                    id: "flavour".into(),
                    value: "quick".into()
                }]),
                false
            )
            .is_ok()
    );
}

#[test]
fn native_agent_execution_binding_and_uncertain_submission_gate_dispatch() {
    let mut initial = state(1);
    initial.submission = Submission::None;
    let mut owner = NativeAgents::default();
    let key = adopt(&mut owner, initial.clone(), "discover");
    assert!(owner.can_submit(&key));
    let binding = initial.execution.as_ref().expect("execution");
    assert_eq!(
        binding.model_choice().provider_id,
        initial.scope.provider_id
    );
    assert_eq!(
        binding.model_choice().variant.as_deref(),
        Some(binding.variant.as_str())
    );
    let mut unbound = initial.clone();
    unbound.revision += 1;
    unbound.execution = None;
    owner.state_changed(&key.server, unbound.clone());
    assert!(!owner.can_submit(&key));
    let mut uncertain = unbound.clone();
    uncertain.revision += 1;
    uncertain.submission = Submission::OutcomeUnknown("unresolved".into());
    uncertain.execution = initial.execution.clone();
    owner.state_changed(&key.server, uncertain);
    assert!(!owner.can_submit(&key));
    let mut replacement = initial.clone();
    replacement.activation_id = "next".into();
    replacement.revision = 0;
    let read = owner
        .begin(key.clone(), "replace".into(), Call::GetState, true)
        .expect("begin");
    owner.complete(
        &read,
        Reply::Success {
            request_id: read.request_id.clone(),
            state: replacement.clone(),
        },
    );
    assert!(
        !owner.can_submit(&key),
        "new activation alone cannot clear an unresolved submission"
    );
    replacement.revision += 1;
    owner.state_changed(&key.server, replacement);
    assert!(
        owner.can_submit(&key),
        "a newer authoritative state in the accepted activation reconciles it"
    );
}

#[test]
fn native_agent_mutations_validate_whole_config_answers_and_never_replay_uncertain_answers() {
    let initial = state(1);
    let mut owner = NativeAgents::default();
    let key = adopt(&mut owner, initial.clone(), "discover");
    assert!(matches!(
        owner.begin(
            key.clone(),
            "bad-config".into(),
            Call::SetConfig(vec![
                ConfigChange {
                    id: "flavour".into(),
                    value: "careful".into()
                },
                ConfigChange {
                    id: "unknown".into(),
                    value: "x".into()
                }
            ]),
            false
        ),
        Err(ErrorCode::InvalidConfig)
    ));
    let answer = Answer::Permission {
        request_id: "permission-fixture".into(),
        choice_id: "yes-once".into(),
    };
    assert!(!validate_answer(
        &initial.requests[0],
        &Answer::Permission {
            request_id: "permission-fixture".into(),
            choice_id: "always".into()
        }
    ));
    let token = owner
        .begin(
            key.clone(),
            "answer-once".into(),
            Call::Answer(answer.clone()),
            false,
        )
        .expect("begin");
    owner.failed(&token);
    assert!(matches!(
        owner.begin(
            key.clone(),
            "answer-again".into(),
            Call::Answer(answer),
            false
        ),
        Err(ErrorCode::OutcomeUnknown)
    ));
    assert!(matches!(
        owner.begin(key.clone(), "answer-once".into(), Call::GetState, true),
        Err(ErrorCode::InvalidRequest)
    ));
    assert!(
        !owner.can_submit(&key),
        "fixture has an unresolved submission"
    );
    let question = &initial.requests[1];
    let field = QuestionValue {
        field_id: "target".into(),
        choice_ids: vec!["preview".into()],
        text: None,
    };
    for values in [
        vec![],
        vec![field.clone(), field.clone()],
        vec![QuestionValue {
            text: Some("bad".into()),
            ..field.clone()
        }],
        vec![QuestionValue {
            choice_ids: vec!["preview".into(), "test".into()],
            ..field.clone()
        }],
    ] {
        assert!(!validate_answer(
            question,
            &Answer::Question {
                request_id: question.id.clone(),
                values
            }
        ));
    }
    assert!(validate_answer(
        question,
        &Answer::Question {
            request_id: question.id.clone(),
            values: vec![field]
        }
    ));
}

#[test]
fn native_agent_progress_checks_location_namespace_live_tool_revision_and_sequence() {
    let fixture = fixture();
    let AgentEvent::ToolProgress(progress) = parse_event(&fixture["progress"]).expect("progress")
    else {
        panic!("progress");
    };
    let mut owner = NativeAgents::default();
    let key = adopt(&mut owner, state(1), "discover");
    let mut authority = ToolAuthority {
        tool: &progress.tool,
        provider_id: &key.scope.provider_id,
        running: true,
        executed: true,
        live: true,
    };
    assert!(owner.tool_progress(&key, progress.clone(), &authority));
    assert_eq!(
        owner.progress_text(&key, &authority),
        Some("Synthetic live progress")
    );
    assert!(!owner.tool_progress(&key, progress.clone(), &authority));
    authority.live = false;
    assert_eq!(owner.progress_text(&key, &authority), None);
    assert!(!owner.tool_progress(&key, progress.clone(), &authority));
    authority.live = true;
    authority.running = false;
    assert_eq!(owner.progress_text(&key, &authority), None);
    authority.running = true;
    authority.executed = false;
    assert_eq!(owner.progress_text(&key, &authority), None);
    authority.executed = true;
    authority.provider_id = "another";
    assert_eq!(owner.progress_text(&key, &authority), None);
    authority.provider_id = &key.scope.provider_id;
    for changed in ["activation", "revision", "call", "scope"] {
        let mut bad = progress.clone();
        bad.sequence += 1;
        match changed {
            "activation" => bad.activation_id = "old".into(),
            "revision" => bad.revision += 1,
            "call" => bad.tool.call_id = "another".into(),
            _ => bad.scope.session_id = Some("another".into()),
        }
        assert!(!owner.tool_progress(&key, bad, &authority));
    }
    owner.invalidate();
    adopt(&mut owner, state(1), "reconnected");
    assert_eq!(owner.progress_text(&key, &authority), None);
    assert!(
        !owner.tool_progress(&key, progress.clone(), &authority),
        "reconnect in the same activation retains sequence watermarks"
    );
    let mut next_progress = progress.clone();
    next_progress.sequence += 1;
    assert!(owner.tool_progress(&key, next_progress, &authority));
    let mut published_payload =
        json!({"location":{"directory":progress.scope.directory},"data":fixture["progress"]});
    let kind = format!(
        "rpc.openchamber.native-agent.{}.v1.toolProgress",
        progress.scope.provider_id
    );
    assert!(published(&kind, &published_payload).is_ok());
    assert!(
        published(
            "rpc.openchamber.native-agent.other.v1.toolProgress",
            &published_payload
        )
        .is_err()
    );
    published_payload["location"]["directory"] = json!("/elsewhere");
    assert!(published(&kind, &published_payload).is_err());
}

#[test]
fn native_agent_opencode_provenance_is_live_executed_and_exact_and_final_content_wins() {
    use crate::event::{ServerEvent, decode_frame};
    use openchamber_native_core::transcript::Transcript;
    let history = json!({"id":"message-fixture","time":{"created":1},"type":"assistant","model":{"providerID":"synthetic-native","id":"synthetic-route"},"content":[{"type":"tool","id":"call-fixture","name":"read","executed":true,"state":{"status":"running","input":{},"metadata":{"provider":"evil-websearch-provider"}}}]});
    let message = crate::projection::message(serde_json::from_value(history).expect("history"))
        .expect("message projection")
        .expect("message");
    let mut transcript = Transcript::from_history(vec![message]);
    let mut owner = NativeAgents::default();
    let key = adopt(&mut owner, state(1), "discover");
    let mut progress = match parse_event(&fixture()["progress"]).expect("progress") {
        AgentEvent::ToolProgress(p) => p,
        _ => unreachable!(),
    };
    // OpenCode's tool.id is both its call identity and projected part identity.
    progress.tool.part_id = progress.tool.call_id.clone();
    let authority = tool_authority(&transcript, &progress.tool).expect("tool authority");
    assert_eq!(authority.provider_id, "synthetic-native");
    assert!(!authority.live);
    assert!(!owner.tool_progress(&key, progress.clone(), &authority));
    let apply = |transcript: &mut Transcript, kind: &str, data: Value| {
        let frame = decode_frame(
            &json!({"type":"event","payload":{"id":"event","created":2,"type":kind,"data":data}})
                .to_string(),
        )
        .expect("event frame");
        for event in frame.events {
            if let ServerEvent::Transcript { event, .. } = event {
                let _ = transcript.apply(event);
            }
        }
    };
    let called = json!({"sessionID":"session-fixture","assistantMessageID":"message-fixture","id":"call-fixture","input":{},"executed":true});
    apply(&mut transcript, "session.tool.called", called.clone());
    let authority = tool_authority(&transcript, &progress.tool).expect("tool authority");
    assert!(authority.live && authority.executed && authority.running);
    assert!(owner.tool_progress(&key, progress.clone(), &authority));
    let mut wrong_call = progress.tool.clone();
    wrong_call.call_id = "other-call".into();
    assert!(tool_authority(&transcript, &wrong_call).is_none());
    let mut success = called;
    success["content"] = json!([{"type":"text","text":"authoritative final"}]);
    apply(&mut transcript, "session.tool.success", success);
    let authority = tool_authority(&transcript, &progress.tool).expect("tool authority");
    assert!(!authority.running);
    assert_eq!(owner.progress_text(&key, &authority), None);
    assert!(
        matches!(&transcript.messages()[0].parts[0].content, openchamber_native_core::transcript::PartContent::Tool { output: Some(text), .. } if text == "authoritative final")
    );
    let published_payload = json!({"type":"event","payload":{"type":"rpc.openchamber.native-agent.synthetic-native.v1.toolProgress","location":{"directory":"/work/synthetic"},"data":fixture()["progress"]}});
    assert!(matches!(
        decode_frame(&published_payload.to_string())
            .expect("event frame")
            .events
            .as_slice(),
        [ServerEvent::NativeAgent(AgentEvent::ToolProgress(_))]
    ));
    let mut wrong = published_payload;
    wrong["payload"]["location"]["directory"] = json!("/another");
    assert!(matches!(
        decode_frame(&wrong.to_string())
            .expect("event frame")
            .events
            .as_slice(),
        [ServerEvent::NativeAgentInvalid { .. }]
    ));
}

#[test]
fn native_agent_newest_discovery_alone_can_establish_or_replace_an_activation() {
    let initial = state(0);
    let key = key(&initial);
    let mut owner = NativeAgents::default();
    let old = owner
        .begin(key.clone(), "old-read".into(), Call::GetState, true)
        .expect("begin");
    let fresh = owner
        .begin(key.clone(), "fresh-read".into(), Call::Prepare, true)
        .expect("begin");
    assert!(!owner.complete(
        &old,
        Reply::Success {
            request_id: old.request_id.clone(),
            state: initial.clone()
        }
    ));
    assert!(owner.state(&key).is_none());
    assert!(owner.complete(
        &fresh,
        Reply::Success {
            request_id: fresh.request_id.clone(),
            state: initial.clone()
        }
    ));
    let stale = owner
        .begin(key.clone(), "stale-read".into(), Call::GetState, false)
        .expect("begin");
    let mut other_activation = initial;
    other_activation.activation_id = "another".into();
    assert!(owner.complete(
        &stale,
        Reply::Success {
            request_id: stale.request_id.clone(),
            state: other_activation
        }
    ));
    assert_eq!(
        owner.state(&key).expect("state").activation_id,
        "devin-activation-1"
    );
}

#[tokio::test]
async fn native_agent_malformed_foreign_and_failed_rpc_answers_preserve_authority() {
    let sdk = fixture()["providers"][0]["state"].clone();
    let (client, recorded) = crate::test_server::serve(Box::new(move |request| {
        let id = request.body["input"]["requestID"]
            .as_str()
            .expect("request ID");
        let mut reply =
            json!({"output":{"protocolVersion":1,"requestID":id,"ok":true,"state":sdk}});
        match id {
            "foreign-scope" => reply["output"]["state"]["scope"]["directory"] = json!("/foreign"),
            "foreign-id" => reply["output"]["requestID"] = json!("other"),
            "bad-version" => reply["output"]["protocolVersion"] = json!(2),
            "located-envelope" => {
                reply = json!({"data":reply["output"],"location":{"directory":"/work"}})
            }
            "server-auth" => return (401, json!({"error":"private"})),
            "rpc-error" => return (500, json!({"type":"rpc.internal","message":"private"})),
            "redirect" => return (302, json!({})),
            _ => unreachable!(),
        }
        (200, reply)
    }))
    .await;
    let mut auth = client.authentication_failures();
    let initial = state(0);
    let key = key(&initial);
    let mut owner = NativeAgents::default();
    adopt(&mut owner, initial.clone(), "seed");
    let marker = discovery(
        "devin",
        Some(&json!({"openchamber":{"nativeAgent":{"protocolVersion":1}}})),
    )
    .expect("discovery");
    let api = NativeAgentApi::new(client, key.clone(), &marker).expect("native agent API");
    for request_id in [
        "foreign-scope",
        "foreign-id",
        "bad-version",
        "located-envelope",
        "server-auth",
        "rpc-error",
        "redirect",
    ] {
        let ticket = owner
            .begin(key.clone(), request_id.into(), Call::GetState, false)
            .expect("begin");
        let error = api.call(&ticket).await.expect_err("invalid reply");
        assert!(!format!("{error:?}").contains("private"));
        owner.failed(&ticket);
        assert_eq!(owner.state(&key), Some(&initial));
    }
    assert!(auth.has_changed().expect("authentication receiver"));
    auth.borrow_and_update();
    assert_eq!(recorded.lock().expect("recorded requests").len(), 7);
}

#[tokio::test]
async fn native_agent_catalog_retains_only_advertised_enabled_providers_without_probing() {
    let (client, recorded) = crate::test_server::serve(Box::new(|request| {
        let data = match request.path.as_str() {
            "/api/model" => json!([]), "/api/model/default" => Value::Null,
            "/api/provider" => json!([
                {"id":"plain"},
                {"id":"synthetic-native","settings":{"openchamber":{"nativeAgent":{"protocolVersion":1}}}},
                {"id":"disabled","activation":"disabled","settings":{"openchamber":{"nativeAgent":{"protocolVersion":1}}}},
                {"id":"future","settings":{"openchamber":{"nativeAgent":{"protocolVersion":2}}}},
                {"id":"invalid","settings":{"openchamber":{"nativeAgent":{"protocolVersion":1,"url":"private"}}}}
            ]),
            other => panic!("unexpected probe {other}"),
        };
        (200, json!({"location":{"directory":"/work/actual"},"data":data}))
    })).await;
    let catalog = client
        .model_catalog("/work/actual")
        .await
        .expect("model catalog");
    assert_eq!(
        catalog
            .native_agents
            .iter()
            .map(NativeAgentProvider::provider_id)
            .collect::<Vec<_>>(),
        ["synthetic-native"]
    );
    assert_eq!(catalog.providers.len(), 5);
    assert_eq!(recorded.lock().expect("recorded requests").len(), 3);
}

#[tokio::test]
async fn native_agent_rpc_uses_captured_auth_directory_envelopes_and_never_retries() {
    let expected = fixture()["providers"][0]["state"].clone();
    let expected_copy = expected.clone();
    let (client, recorded) = crate::test_server::serve(Box::new(move |request| {
        if request.path.ends_with("/getState") { (200, json!({"output":{"protocolVersion":1,"requestID":"read-1","ok":true,"state":expected_copy}})) }
        else { (500, json!({"error":"private service output"})) }
    })).await;
    let state = parse_state(&expected).expect("SDK state");
    let key = key(&state);
    let marker = discovery(
        "devin",
        Some(&json!({"openchamber":{"nativeAgent":{"protocolVersion":1}}})),
    )
    .expect("discovery");
    let api = NativeAgentApi::new(client, key.clone(), &marker).expect("native agent API");
    let mut owner = NativeAgents::default();
    let ticket = owner
        .begin(key.clone(), "read-1".into(), Call::GetState, true)
        .expect("begin");
    let request = api
        .request(&ticket)
        .expect("request")
        .build()
        .expect("build request");
    assert_eq!(request.method(), Method::POST);
    assert_eq!(
        request.url().path(),
        "/api/rpc/openchamber.native-agent.devin.v1/getState"
    );
    assert_eq!(
        request.headers()["x-opencode-directory"],
        encode_directory(&key.scope.directory)
    );
    assert_eq!(
        request.headers()["authorization"],
        "Bearer oc_client_synthetic"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(
            request
                .body()
                .expect("body")
                .as_bytes()
                .expect("body bytes")
        )
        .expect("JSON body"),
        json!({"input":parameters(&ticket).expect("parameters")})
    );
    let reply = api.call(&ticket).await.expect("reply");
    owner.complete(&ticket, reply);
    let mutation = owner
        .begin(
            key.clone(),
            "config-1".into(),
            Call::SetConfig(vec![ConfigChange {
                id: "runtime-model".into(),
                value: "opaque-model-b".into(),
            }]),
            false,
        )
        .expect("begin");
    assert!(matches!(
        api.call(&mutation).await,
        Err(TransportError::Http { status: 500 })
    ));
    assert_eq!(
        recorded.lock().expect("recorded requests").len(),
        2,
        "mutations never retry"
    );
    let mut wrong = mutation.clone();
    wrong.key.scope.directory = "/elsewhere".into();
    assert!(api.call(&wrong).await.is_err());
    assert_eq!(recorded.lock().expect("recorded requests").len(), 2);
}
