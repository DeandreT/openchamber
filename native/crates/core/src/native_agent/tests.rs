use super::*;

#[test]
fn session_first_draft_authority_never_authorizes_authentication_or_submission() {
    let mut draft = state();
    draft.scope.session_id = None;
    draft.capabilities.prepare_after_session = true;
    draft.status = Status::Unprepared;
    draft.authentication = Authentication::Unknown;
    draft.execution = None;
    let key = OwnerKey {
        server: "server".into(),
        scope: draft.scope.clone(),
    };
    let mut owner = NativeAgents::default();
    let ticket = owner
        .begin(key.clone(), "draft".into(), Call::Prepare, true)
        .expect("test fixture");
    owner.complete(
        &ticket,
        Reply::Success {
            request_id: ticket.request_id.clone(),
            state: draft,
        },
    );
    assert!(owner.can_prepare_after_session(&key));
    assert!(owner.can_set_config(&key));
    assert!(!owner.can_authenticate(&key));
    assert!(!owner.can_submit(&key));
    assert!(matches!(
        owner.begin(key.clone(), "login".into(), Call::Authenticate, false),
        Err(ErrorCode::NotPrepared)
    ));
    owner.invalidate();
    assert!(
        !owner.can_prepare_after_session(&key),
        "a retained snapshot is not draft creation authority"
    );
}

#[test]
fn session_first_partial_or_stale_state_cannot_authorize_submission_as_a_complete_catalog() {
    let mut ready = state();
    ready.capabilities.prepare_after_session = true;
    ready.config_complete = true;
    let key = OwnerKey {
        server: "server".into(),
        scope: ready.scope.clone(),
    };
    let mut owner = NativeAgents::default();
    let ticket = owner
        .begin(key.clone(), "complete".into(), Call::GetState, true)
        .expect("test fixture");
    owner.complete(
        &ticket,
        Reply::Success {
            request_id: ticket.request_id.clone(),
            state: ready.clone(),
        },
    );
    assert!(owner.can_submit(&key));
    let mut partial = ready.clone();
    partial.revision += 1;
    partial.config_complete = false;
    partial.execution = None;
    assert!(owner.state_changed("server", partial.clone()));
    assert!(!owner.can_submit(&key));
    assert!(!owner.state_changed("server", ready.clone()));
    let mut same_revision = partial;
    same_revision.config_complete = true;
    same_revision.execution = ready.execution.clone();
    assert!(!owner.state_changed("server", same_revision));
    assert!(
        !owner.state(&key).expect("test fixture").config_complete,
        "an older complete snapshot cannot reauthorize a newer partial one"
    );
    assert_eq!(
        owner.state(&key).expect("test fixture").config,
        ready.config,
        "withdrawal of discovery authority preserves previously exposed choices"
    );
}

fn authority_owner(mut initial: State) -> (NativeAgents, OwnerKey, State) {
    initial.execution = None;
    initial.authentication = Authentication::Required;
    initial.requests.push(PendingRequest {
        id: "approval".into(),
        title: "Synthetic approval".into(),
        description: String::new(),
        kind: RequestKind::Permission {
            choices: vec![PermissionChoice {
                id: "allow".into(),
                label: "Allow".into(),
                decision: Decision::Allow,
            }],
        },
    });
    let key = OwnerKey {
        server: "server".into(),
        scope: initial.scope.clone(),
    };
    let mut owner = NativeAgents::default();
    let seed = owner
        .begin(key.clone(), "seed".into(), Call::GetState, true)
        .expect("test fixture");
    assert!(owner.complete(
        &seed,
        Reply::Success {
            request_id: seed.request_id.clone(),
            state: initial.clone(),
        }
    ));
    (owner, key, initial)
}

fn assert_authority(owner: &NativeAgents, key: &OwnerKey, expected: bool) {
    assert_eq!(owner.can_answer(key, "approval"), expected);
    assert_eq!(owner.can_set_config(key), expected);
    assert_eq!(owner.can_authenticate(key), expected);
}

#[test]
fn native_agent_authority_queries_work_without_execution_or_authenticated_submission() {
    let (mut owner, key, _) = authority_owner(state());
    assert_authority(&owner, &key, true);
    assert!(!owner.can_submit(&key));
    assert!(!owner.can_answer(&key, "missing"));
    assert!(!owner.can_answer(&key, ""));
    let mut other = key.clone();
    other.server = "other".into();
    assert_authority(&owner, &other, false);
    other = key.clone();
    other.scope.session_id = Some("other-session".into());
    assert_authority(&owner, &other, false);
    let read = owner
        .begin(key.clone(), "read".into(), Call::GetState, false)
        .expect("test fixture");
    assert_authority(&owner, &key, true);
    owner.failed(&read);
    assert_authority(&owner, &key, true);
    owner.invalidate();
    assert_authority(&owner, &key, false);
}

#[test]
fn native_agent_authority_queries_enforce_each_capability_and_current_native_request() {
    for denied in ["answer", "config", "authentication", "owner", "request"] {
        let (mut owner, key, mut next) = authority_owner(state());
        next.revision += 1;
        match denied {
            "answer" => next.capabilities.answer = false,
            "config" => next.capabilities.set_config = false,
            "authentication" => next.capabilities.authenticate = false,
            "owner" => next.permission_owner = PermissionOwner::OpenCode,
            "request" => next.requests.clear(),
            _ => unreachable!(),
        }
        assert!(owner.state_changed("server", next));
        assert_eq!(
            owner.can_answer(&key, "approval"),
            !matches!(denied, "answer" | "owner" | "request")
        );
        assert_eq!(owner.can_set_config(&key), denied != "config");
        assert_eq!(owner.can_authenticate(&key), denied != "authentication");
    }
}

#[test]
fn native_agent_authority_queries_block_mutations_and_preserve_uncertain_answers() {
    let (mut owner, key, mut initial) = authority_owner(state());
    let answer = owner
        .begin(
            key.clone(),
            "answer".into(),
            Call::Answer(Answer::Permission {
                request_id: "approval".into(),
                choice_id: "allow".into(),
            }),
            false,
        )
        .expect("test fixture");
    assert_authority(&owner, &key, false);
    owner.failed(&answer);
    assert_authority(&owner, &key, false);
    let read = owner
        .begin(key.clone(), "reconcile".into(), Call::GetState, false)
        .expect("test fixture");
    owner.complete(
        &read,
        Reply::Success {
            request_id: read.request_id.clone(),
            state: initial.clone(),
        },
    );
    assert!(!owner.can_answer(&key, "approval"));
    assert!(owner.can_set_config(&key));
    assert!(owner.can_authenticate(&key));
    initial.revision += 1;
    initial.requests.clear();
    assert!(owner.state_changed("server", initial.clone()));
    assert!(!owner.can_answer(&key, "approval"));
    initial.revision += 1;
    initial.requests.push(PendingRequest {
        id: "new-approval".into(),
        title: "New approval".into(),
        description: String::new(),
        kind: RequestKind::Permission {
            choices: Vec::new(),
        },
    });
    assert!(owner.state_changed("server", initial));
    assert!(owner.can_answer(&key, "new-approval"));
}

#[test]
fn native_agent_authority_queries_withdraw_on_stale_activation_and_inflight_config_or_auth() {
    for call in [
        Call::Authenticate,
        Call::SetConfig(vec![ConfigChange {
            id: "runtime-choice".into(),
            value: "build".into(),
        }]),
    ] {
        let (mut owner, key, initial) = authority_owner(state());
        let mutation = owner
            .begin(key.clone(), "mutate".into(), call, false)
            .expect("test fixture");
        assert_authority(&owner, &key, false);
        owner.complete(
            &mutation,
            Reply::Failure {
                request_id: mutation.request_id.clone(),
                code: ErrorCode::StaleActivation,
                outcome: Outcome::Rejected,
                state: None,
            },
        );
        assert_authority(&owner, &key, false);
        let discovery = owner
            .begin(key.clone(), "rediscover".into(), Call::GetState, true)
            .expect("test fixture");
        assert_authority(&owner, &key, false);
        owner.complete(
            &discovery,
            Reply::Success {
                request_id: discovery.request_id.clone(),
                state: initial,
            },
        );
        assert_authority(&owner, &key, true);
    }
}

fn state() -> State {
    State {
        scope: Scope {
            directory: "/work/actual".into(),
            provider_id: "independent".into(),
            session_id: Some("session".into()),
        },
        activation_id: "activation".into(),
        revision: 4,
        status: Status::Ready,
        authentication: Authentication::Authenticated,
        permission_owner: PermissionOwner::Native,
        capabilities: Capabilities {
            prepare_after_session: false,
            authenticate: true,
            set_config: true,
            answer: true,
            tool_progress: true,
        },
        config_complete: false,
        config: vec![Config {
            id: "runtime-choice".into(),
            label: "Mode".into(),
            role: ConfigRole::Mode,
            value: "plan".into(),
            choices: vec![
                Choice {
                    id: "plan".into(),
                    label: "Plan".into(),
                },
                Choice {
                    id: "build".into(),
                    label: "Build".into(),
                },
            ],
        }],
        requests: Vec::new(),
        submission: Submission::None,
        diagnostic: None,
        execution: Some(ExecutionBinding {
            provider_id: "independent".into(),
            model_id: "cloud".into(),
            variant: "immutable-snapshot".into(),
        }),
    }
}

#[test]
fn native_agent_ready_binding_requires_current_generation_and_nonstale_snapshot() {
    let initial = state();
    let key = OwnerKey {
        server: "server".into(),
        scope: initial.scope.clone(),
    };
    let mut owner = NativeAgents::default();
    assert!(!owner.state_changed("server", initial.clone()));
    let ticket = owner
        .begin(key.clone(), "discover".into(), Call::Prepare, true)
        .expect("test fixture");
    owner.complete(
        &ticket,
        Reply::Success {
            request_id: ticket.request_id.clone(),
            state: initial.clone(),
        },
    );
    assert!(owner.can_submit(&key));
    owner.invalidate();
    assert!(!owner.can_submit(&key));
    let mut stale = initial.clone();
    stale.revision -= 1;
    let read = owner
        .begin(key.clone(), "old".into(), Call::GetState, true)
        .expect("test fixture");
    owner.complete(
        &read,
        Reply::Success {
            request_id: read.request_id.clone(),
            state: stale,
        },
    );
    assert!(!owner.can_submit(&key));
    let read = owner
        .begin(key.clone(), "equal".into(), Call::GetState, true)
        .expect("test fixture");
    owner.complete(
        &read,
        Reply::Success {
            request_id: read.request_id.clone(),
            state: initial.clone(),
        },
    );
    assert!(owner.can_submit(&key));
    let mut no_binding = initial;
    no_binding.revision += 1;
    no_binding.execution = None;
    assert!(owner.state_changed("server", no_binding));
    assert!(!owner.can_submit(&key));
}

#[test]
fn native_agent_config_uncertainty_requires_authoritative_read_before_another_mutation_or_send() {
    let initial = state();
    let key = OwnerKey {
        server: "server".into(),
        scope: initial.scope.clone(),
    };
    let mut owner = NativeAgents::default();
    let read = owner
        .begin(key.clone(), "read".into(), Call::GetState, true)
        .expect("test fixture");
    owner.complete(
        &read,
        Reply::Success {
            request_id: read.request_id.clone(),
            state: initial.clone(),
        },
    );
    let changes = vec![ConfigChange {
        id: "runtime-choice".into(),
        value: "build".into(),
    }];
    let mutation = owner
        .begin(
            key.clone(),
            "write".into(),
            Call::SetConfig(changes.clone()),
            false,
        )
        .expect("test fixture");
    assert!(!owner.can_submit(&key));
    owner.failed(&mutation);
    assert_eq!(owner.state(&key), Some(&initial));
    assert!(!owner.can_submit(&key));
    assert_eq!(
        owner.begin(key.clone(), "retry".into(), Call::SetConfig(changes), false),
        Err(ErrorCode::OutcomeUnknown)
    );
    let read = owner
        .begin(key.clone(), "reconcile".into(), Call::GetState, false)
        .expect("test fixture");
    owner.complete(
        &read,
        Reply::Success {
            request_id: read.request_id.clone(),
            state: initial,
        },
    );
    assert!(owner.can_submit(&key));
}

#[test]
fn native_agent_question_validation_matches_ecmascript_required_text_and_rejects_duplicates() {
    let request = PendingRequest {
        id: "question".into(),
        title: "Question".into(),
        description: String::new(),
        kind: RequestKind::Question {
            fields: vec![QuestionField {
                id: "text".into(),
                label: "Text".into(),
                required: true,
                multiple: false,
                allow_text: true,
                choices: Vec::new(),
            }],
        },
    };
    let answer = |text: &str| Answer::Question {
        request_id: "question".into(),
        values: vec![QuestionValue {
            field_id: "text".into(),
            choice_ids: Vec::new(),
            text: Some(text.into()),
        }],
    };
    assert!(
        !validate_answer(&request, &answer("\u{feff}")),
        "ECMAScript trims BOM"
    );
    assert!(
        validate_answer(&request, &answer("\u{85}")),
        "ECMAScript retains U+0085"
    );
    let mut duplicate = answer("yes");
    if let Answer::Question { values, .. } = &mut duplicate {
        values.push(values[0].clone());
    }
    assert!(!validate_answer(&request, &duplicate));
    assert!(!format!("{:?}", answer("private answer")).contains("private answer"));
    assert!(!format!("{request:?}").contains("Question"));
}

#[test]
fn native_agent_reads_started_before_uncertainty_cannot_clear_its_barrier() {
    for (order, domain_unknown) in [
        ("before-mutation", false),
        ("during-mutation", false),
        ("before-failure", false),
        ("before-mutation", true),
        ("during-mutation", true),
        ("before-failure", true),
    ] {
        let initial = state();
        let key = OwnerKey {
            server: "server".into(),
            scope: initial.scope.clone(),
        };
        let mut owner = NativeAgents::default();
        let seed = owner
            .begin(key.clone(), "seed".into(), Call::GetState, true)
            .expect("test fixture");
        owner.complete(
            &seed,
            Reply::Success {
                request_id: seed.request_id.clone(),
                state: initial.clone(),
            },
        );
        let early_read = if order == "before-mutation" {
            Some(
                owner
                    .begin(key.clone(), "early-read".into(), Call::GetState, false)
                    .expect("test fixture"),
            )
        } else {
            None
        };
        let mutation = owner
            .begin(
                key.clone(),
                "mutate".into(),
                Call::SetConfig(vec![ConfigChange {
                    id: "runtime-choice".into(),
                    value: "build".into(),
                }]),
                false,
            )
            .expect("test fixture");
        let read = early_read.unwrap_or_else(|| {
            owner
                .begin(key.clone(), "early-read".into(), Call::GetState, false)
                .expect("test fixture")
        });
        let reply = Reply::Success {
            request_id: read.request_id.clone(),
            state: initial.clone(),
        };
        if order == "before-failure" {
            owner.complete(&read, reply.clone());
        }
        if domain_unknown {
            owner.complete(
                &mutation,
                Reply::Failure {
                    request_id: mutation.request_id.clone(),
                    code: ErrorCode::OutcomeUnknown,
                    outcome: Outcome::Unknown,
                    state: None,
                },
            );
        } else {
            owner.failed(&mutation);
        }
        if order != "before-failure" {
            owner.complete(&read, reply);
        }
        assert!(!owner.can_submit(&key), "{order}");
        assert_eq!(
            owner.begin(
                key.clone(),
                "another-mutation".into(),
                Call::Authenticate,
                false
            ),
            Err(ErrorCode::OutcomeUnknown)
        );
        let reconcile = owner
            .begin(
                key.clone(),
                "post-uncertainty".into(),
                Call::GetState,
                false,
            )
            .expect("test fixture");
        owner.complete(
            &reconcile,
            Reply::Success {
                request_id: reconcile.request_id.clone(),
                state: initial,
            },
        );
        assert!(owner.can_submit(&key));
    }
}

#[test]
fn native_agent_stale_activation_withdraws_actions_and_progress_until_new_discovery() {
    for attached in [false, true] {
        let initial = state();
        let key = OwnerKey {
            server: "server".into(),
            scope: initial.scope.clone(),
        };
        let mut owner = NativeAgents::default();
        let seed = owner
            .begin(key.clone(), "seed".into(), Call::GetState, true)
            .expect("test fixture");
        owner.complete(
            &seed,
            Reply::Success {
                request_id: seed.request_id.clone(),
                state: initial.clone(),
            },
        );
        let tool = ToolIdentity {
            message_id: "message".into(),
            part_id: "call".into(),
            call_id: "call".into(),
        };
        let authority = ToolAuthority {
            tool: &tool,
            provider_id: "independent",
            live: true,
            executed: true,
            running: true,
        };
        let progress = ToolProgress {
            scope: key.scope.clone(),
            activation_id: initial.activation_id.clone(),
            revision: initial.revision,
            tool: tool.clone(),
            sequence: 1,
            text: "progress".into(),
        };
        assert!(owner.tool_progress(&key, progress.clone(), &authority));
        let old_read = owner
            .begin(key.clone(), "old-read".into(), Call::GetState, false)
            .expect("test fixture");
        let mutation = owner
            .begin(key.clone(), "mutate".into(), Call::Authenticate, false)
            .expect("test fixture");
        let mut replacement = initial.clone();
        replacement.activation_id = "replacement".into();
        replacement.revision = 0;
        owner.complete(
            &mutation,
            Reply::Failure {
                request_id: mutation.request_id.clone(),
                code: ErrorCode::StaleActivation,
                outcome: Outcome::Rejected,
                state: attached.then_some(replacement.clone()),
            },
        );
        assert_eq!(owner.state(&key), Some(&initial));
        assert!(
            !owner.can_submit(&key),
            "stale activation with attached={attached}"
        );
        assert_eq!(owner.progress_text(&key, &authority), None);
        let mut next_progress = progress;
        next_progress.sequence += 1;
        assert!(!owner.tool_progress(&key, next_progress, &authority));
        assert!(!owner.complete(
            &old_read,
            Reply::Success {
                request_id: old_read.request_id.clone(),
                state: initial.clone()
            }
        ));
        assert!(!owner.state_changed("server", replacement.clone()));
        assert!(
            owner
                .begin(key.clone(), "bound-recovery".into(), Call::GetState, false)
                .is_err()
        );
        assert!(
            owner
                .begin(key.clone(), "no-auth".into(), Call::Authenticate, false)
                .is_err()
        );
        let discover = owner
            .begin(key.clone(), "new-discovery".into(), Call::GetState, true)
            .expect("test fixture");
        owner.complete(
            &discover,
            Reply::Success {
                request_id: discover.request_id.clone(),
                state: replacement.clone(),
            },
        );
        assert_eq!(owner.state(&key), Some(&replacement));
        assert!(owner.can_submit(&key));
    }
}
