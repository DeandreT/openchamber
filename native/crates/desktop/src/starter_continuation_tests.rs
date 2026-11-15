use super::*;
use gpui_kit::{AppContext, TestAppContext};
use openchamber_native_core::native_agent::{
    Authentication, Call, Capabilities, Choice, Config, ConfigRole, ExecutionBinding, OwnerKey,
    PermissionOwner, Reply, Scope, State, Status, Submission,
};
use openchamber_native_core::send_config::{ModelChoice, Selection};

fn capture(owner: DraftKey, project: String, provider: &str) -> StarterCapture {
    StarterCapture {
        id: MessageId::from("msg_starter-activation"),
        owner,
        project,
        draft: Draft::new("original 😀 text"),
        preset_text: "/explore original 😀 text".into(),
        selection: Selection {
            model: Some(ModelChoice {
                provider_id: provider.into(),
                model_id: "original-model".into(),
                variant: Some("original-binding".into()),
            }),
            agent_name: Some("original-agent".into()),
        },
        parts: PromptParts::default(),
        permission_mode: None,
    }
}

fn ready_native(owner: &DraftKey) -> (OwnerKey, State) {
    let scope = Scope {
        directory: owner.directory().to_owned(),
        provider_id: "native-provider".into(),
        session_id: None,
    };
    let key = OwnerKey {
        server: owner.runtime().as_str().to_owned(),
        scope: scope.clone(),
    };
    let state = State {
        scope,
        activation_id: "activation".into(),
        revision: 1,
        status: Status::Ready,
        authentication: Authentication::Authenticated,
        permission_owner: PermissionOwner::Native,
        capabilities: Capabilities {
            prepare_after_session: false,
            authenticate: true,
            set_config: true,
            answer: true,
            tool_progress: false,
        },
        config_complete: false,
        config: vec![Config {
            id: "mode".into(),
            label: "Mode".into(),
            role: ConfigRole::Mode,
            value: "build".into(),
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
            provider_id: "native-provider".into(),
            model_id: "published-model".into(),
            variant: "published-binding".into(),
        }),
    };
    (key, state)
}

fn publish(this: &mut Workspace, key: &OwnerKey, state: State) {
    let ticket = this
        .native_agents
        .owner
        .begin(
            key.clone(),
            format!("ready-{}", state.revision),
            Call::Prepare,
            true,
        )
        .expect("begin");
    assert!(this.native_agents.owner.complete(
        &ticket,
        Reply::Success {
            request_id: ticket.request_id.clone(),
            state,
        }
    ));
}

#[gpui_kit::test]
fn session_first_starter_creates_from_its_capture_without_waiting_for_a_draft_binding(
    cx: &mut TestAppContext,
) {
    let (_fixture, window, workspace) = open_ready(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            let project = this.topology.projects[0].clone();
            this.show_new_session(window, cx);
            this.choose_draft_project(&project.path, window, cx);
            let original = capture(
                this.composer_key.clone().expect("composer key"),
                project.id.to_string(),
                "native-provider",
            );
            let (key, mut state) = ready_native(&original.owner);
            state.status = Status::Unprepared;
            state.authentication = Authentication::Unknown;
            state.execution = None;
            state.capabilities.prepare_after_session = true;
            publish(this, &key, state);
            let config = this
                .capture_native_creation(original.selection.model.as_ref().expect("model"))
                .expect("native creation");
            assert!(config.prepare_after_session);
            assert!(config.variant.is_none());
            assert!(!this.native_agents.owner.can_submit(&key));
            let id = original.id.clone();
            this.drafts
                .save(original.owner.clone(), original.draft.clone());
            assert!(
                this.starters
                    .captures
                    .begin(original.clone(), &mut this.drafts)
            );
            this.restore_composer_draft(window, cx);
            this.starters.continuations.insert(
                id.clone(),
                StarterContinuation {
                    window: window.window_handle(),
                    rendered: Some(("visible prompt".into(), "captured instructions".into())),
                    effort: None,
                    native: Some(StarterNative {
                        preferences: Some(config.config.clone()),
                        config: Some(config.clone()),
                        preparation: None,
                    }),
                },
            );
            this.resume_starter_activation(window, cx);
            assert!(this.starters.captures.get(&id).is_none());
            let (_, creation) = this.creations.iter().next().expect("creation");
            assert_eq!(creation.source, original.owner);
            assert_eq!(creation.native_config.as_ref(), Some(&config));
            assert_eq!(creation.prompt.agent, original.selection.agent_name);
            assert_eq!(creation.prompt.model, original.selection.model);
            assert_eq!(creation.prompt.text, "visible prompt");
            assert_eq!(
                creation.prompt.parts.context[0].text,
                "captured instructions"
            );
            this.validate_queue_intent_admission()
                .expect("session-first starter is durable before any creation request");
        });
    })
    .expect("window");
    crate::recovery_tests::close(cx, window, &workspace);
}

#[gpui_kit::test]
fn session_first_loading_starter_promotes_captured_intent_when_hidden_metadata_arrives(
    cx: &mut TestAppContext,
) {
    for preferences in [None, Some(vec![("mode".into(), "plan".into())])] {
        let (_fixture, window, workspace) = open_ready(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                let project = this.topology.projects[0].clone();
                this.show_new_session(window, cx);
                this.choose_draft_project(&project.path, window, cx);
                let mut original = capture(
                    this.composer_key.clone().expect("composer key"),
                    project.id.to_string(),
                    "native-provider",
                );
                original.selection.model.as_mut().expect("model").variant = None;
                let id = original.id.clone();
                this.drafts
                    .save(original.owner.clone(), original.draft.clone());
                assert!(
                    this.starters
                        .captures
                        .begin(original.clone(), &mut this.drafts)
                );
                this.restore_composer_draft(window, cx);
                this.starters.continuations.insert(
                    id.clone(),
                    StarterContinuation {
                        window: window.window_handle(),
                        rendered: Some(("visible prompt".into(), "captured instructions".into())),
                        effort: None,
                        native: Some(StarterNative {
                            preferences: preferences.clone(),
                            config: None,
                            preparation: None,
                        }),
                    },
                );
                // Metadata arrives while another directory/composer is visible.
                this.composer_key = DraftKey::new(
                    this.runtime.clone(),
                    "/another",
                    original.owner.target().clone(),
                );
                let (key, mut state) = ready_native(&original.owner);
                state.status = Status::Unprepared;
                state.authentication = Authentication::Unknown;
                state.execution = None;
                state.capabilities.prepare_after_session = true;
                publish(this, &key, state);
                this.retain_starter_native_ready(&key, cx);
                let frozen = this.starters.continuations[&id]
                    .native
                    .as_ref()
                    .expect("test fixture")
                    .config
                    .as_ref()
                    .expect("late config-only authority promotes accepted starter");
                assert!(frozen.prepare_after_session);
                assert_eq!(frozen.model_id, "original-model");
                assert_eq!(
                    frozen.config,
                    preferences
                        .clone()
                        .unwrap_or_else(|| vec![("mode".into(), "build".into())])
                );
                assert_eq!(
                    this.starters.captures.get(&id).expect("capture").selection,
                    original.selection
                );
                assert!(!this.native_agents.owner.can_submit(&key));
                this.composer_key = Some(original.owner.clone());
                this.resume_starter_activation(window, cx);
                assert!(this.starters.captures.get(&id).is_none());
                let (_, pending) = this
                    .creations
                    .iter()
                    .next()
                    .expect("accepted starter creates without draft execution");
                assert_eq!(pending.source, original.owner);
                assert_eq!(pending.prompt.agent, original.selection.agent_name);
                assert_eq!(pending.prompt.model, original.selection.model);
                assert_eq!(pending.prompt.text, "visible prompt");
                this.validate_queue_intent_admission()
                    .expect("promoted intent is durable");
            });
        })
        .expect("window");
        crate::recovery_tests::close(cx, window, &workspace);
    }
}

fn open_ready(
    cx: &mut TestAppContext,
) -> (
    crate::recovery_tests::Fixture,
    gpui_kit::AnyWindowHandle,
    gpui_kit::Entity<Workspace>,
) {
    use crate::connection::{Connection, ConnectionState, ServerConfig};
    use openchamber_native_transport::credential::Credential;
    use openchamber_native_transport::{client::ServerClient, endpoint::Endpoint};

    let fixture = crate::recovery_tests::Fixture::new();
    cx.executor().allow_parking();
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            let endpoint = Endpoint::parse("http://localhost:9").expect("endpoint");
            let mut connection =
                Connection::new(ServerConfig::for_endpoint(endpoint.clone())).expect("connection");
            connection.client =
                Some(ServerClient::new(endpoint, Credential::None).expect("client"));
            connection.state = ConnectionState::Connected;
            this.connection = Some(connection);
            this.open_persistence(fixture.journal(), window, cx);
        })
    })
    .expect("window");
    crate::recovery_tests::ready(cx, &workspace);
    (fixture, window, workspace)
}

#[gpui_kit::test]
fn captured_readiness_ignores_unavailable_current_selection_and_checks_original_owner(
    cx: &mut TestAppContext,
) {
    let (_fixture, window, workspace) = open_ready(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            let project = this.topology.projects[0].clone();
            this.show_new_session(window, cx);
            this.choose_draft_project(&project.path, window, cx);
            let owner = this.composer_key.clone().expect("composer key");
            let mut original = capture(owner.clone(), project.id.to_string(), "original-provider");
            let selection_owner = this.selection_owner().expect("selection owner");
            this.composer_selections.forget(&selection_owner);
            this.composer_selections.restore_if_absent(
                selection_owner,
                Selection {
                    model: Some(ModelChoice {
                        provider_id: "later-unavailable-provider".into(),
                        model_id: "later-model".into(),
                        variant: None,
                    }),
                    agent_name: Some("later-agent".into()),
                },
            );
            // Current model discovery fails after the later choice. The saved send
            // still has its complete original model selection.
            this.reset_send_catalog();
            assert!(!this.can_create_connected());
            assert!(this.can_create_starter(&original));

            let (native_key, state) = ready_native(&owner);
            original.selection.model = state
                .execution
                .as_ref()
                .map(|execution| execution.model_choice());
            publish(this, &native_key, state);
            assert!(!this.can_create_connected());
            assert!(this.can_create_starter(&original));
            this.native_agents.owner.invalidate();
            assert!(
                !this.can_create_starter(&original),
                "native authority must be current"
            );

            original
                .selection
                .model
                .as_mut()
                .expect("model")
                .provider_id = "original-provider".into();
            this.composer_key =
                DraftKey::new(this.runtime.clone(), "/another", owner.target().clone());
            assert!(
                !this.can_create_starter(&original),
                "another composer cannot resume this activation"
            );
            this.open_new_session(crate::chat_draft::NewSession::Chat, window, cx);
            original.owner = this.composer_key.clone().expect("composer key");
            original.project = openchamber_native_core::chat_draft::CHAT_PROJECT_ID.into();
            assert!(
                !this.can_create_starter(&original),
                "a named Chat directory is not prepared"
            );
        })
    })
    .expect("window");
    crate::recovery_tests::close(cx, window, &workspace);
}

#[gpui_kit::test]
fn hidden_native_completion_freezes_original_preferences_and_survives_runtime_reset(
    cx: &mut TestAppContext,
) {
    let (window, workspace) = crate::workspace_tests::open(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            let owner = DraftKey::new(
                this.runtime.clone(),
                "/original",
                DraftTarget::NewSession("draft".into()),
            )
            .expect("test fixture");
            let original = capture(owner.clone(), "project".into(), "native-provider");
            let id = original.id.clone();
            this.drafts.save(owner.clone(), original.draft.clone());
            assert!(this.starters.captures.begin(original, &mut this.drafts));
            this.starters.continuations.insert(
                id.clone(),
                StarterContinuation {
                    window: window.window_handle(),
                    rendered: Some(("visible".into(), "instructions".into())),
                    effort: None,
                    native: Some(StarterNative {
                        preferences: Some(vec![("mode".into(), "plan".into())]),
                        config: None,
                        preparation: Some(1),
                    }),
                },
            );
            assert_ne!(this.composer_key.as_ref(), Some(&owner));
            let (key, mut state) = ready_native(&owner);
            publish(this, &key, state.clone());
            this.retain_starter_native_ready(&key, cx);
            let frozen = this.starters.continuations[&id]
                .native
                .as_ref()
                .expect("test fixture")
                .config
                .clone()
                .expect("test fixture");
            assert_eq!(frozen.config, vec![("mode".into(), "plan".into())]);
            assert_eq!(frozen.variant.as_deref(), Some("published-binding"));
            assert_eq!(
                this.starters
                    .captures
                    .get(&id)
                    .expect("test fixture")
                    .selection
                    .agent_name
                    .as_deref(),
                Some("original-agent")
            );

            state.revision += 1;
            state.execution.as_mut().expect("execution").variant = "later-binding".into();
            assert!(this.native_agents.owner.state_changed(&key.server, state));
            this.retain_starter_native_ready(&key, cx);
            this.starters.reset();
            assert!(this.starters.capture_pending(&id));
            assert_eq!(
                this.starters.continuations[&id]
                    .native
                    .as_ref()
                    .expect("test fixture")
                    .config
                    .as_ref(),
                Some(&frozen)
            );
            assert_eq!(
                this.starters.continuations[&id]
                    .rendered
                    .as_ref()
                    .expect("test fixture")
                    .0,
                "visible"
            );
            assert_eq!(
                this.starters.captures.get(&id).expect("capture").owner,
                owner
            );
            this.starters.discard_captures(&owner);
            assert!(!this.starters.capture_pending(&id));
        })
    })
    .expect("window");
}

#[test]
fn native_preparation_failure_keeps_authentication_waiting_distinct() {
    use openchamber_native_core::native_agent::ErrorCode;
    let owner = DraftKey::new(
        "server".into(),
        "/original",
        DraftTarget::NewSession("draft".into()),
    )
    .expect("window");
    let (_, mut state) = ready_native(&owner);
    state.authentication = Authentication::Required;
    state.execution = None;
    assert!(!native_preparation_failed(Some(Ok(())), Some(&state)));
    assert!(!native_preparation_failed(
        Some(Err(ErrorCode::AuthenticationRequired)),
        Some(&state)
    ));
    assert!(
        native_preparation_failed(None, Some(&state)),
        "transport failure retires activation"
    );
    assert!(native_preparation_failed(
        Some(Err(ErrorCode::Unavailable)),
        Some(&state)
    ));
    for status in [Status::Error, Status::Stopped] {
        state.status = status;
        assert!(native_preparation_failed(Some(Ok(())), Some(&state)));
    }
}

#[gpui_kit::test]
fn native_failure_restores_full_capture_even_when_its_owner_is_hidden(cx: &mut TestAppContext) {
    use openchamber_native_core::draft_content::{DraftContext, DraftFile, DraftFileContent};
    use openchamber_native_core::native_agent::AgentEvent;

    let (_fixture, window, workspace) = open_ready(cx);
    for hidden in [false, true] {
        let mut expected = None;
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                let project = this.topology.projects[0].clone();
                this.show_new_session(window, cx);
                this.choose_draft_project(&project.path, window, cx);
                let owner = this.composer_key.clone().expect("composer key");
                let mut original =
                    capture(owner.clone(), project.id.to_string(), "native-provider");
                original.draft.selection = 9..13;
                original.draft.content.files.push(DraftFile {
                    id: "retained-file".into(),
                    name: Some("notes.txt".into()),
                    media_type: Some("text/plain".into()),
                    byte_len: Some(3),
                    source_document_id: None,
                    content: DraftFileContent::Reference {
                        uri: "file:///retained/notes.txt".into(),
                    },
                });
                original.draft.content.context.push(DraftContext {
                    id: "retained-context".into(),
                    text: "quoted λ".into(),
                    description: Some("selection".into()),
                    metadata_json: None,
                    instructions: Some("keep instructions".into()),
                });
                let id = original.id.clone();
                this.drafts.save(owner.clone(), original.draft.clone());
                assert!(
                    this.starters
                        .captures
                        .begin(original.clone(), &mut this.drafts)
                );
                this.restore_composer_draft(window, cx);
                this.starters.continuations.insert(
                    id.clone(),
                    StarterContinuation {
                        window: window.window_handle(),
                        rendered: None,
                        effort: None,
                        native: Some(StarterNative {
                            preferences: None,
                            config: None,
                            preparation: Some(1),
                        }),
                    },
                );
                this.starters.capture_tasks.insert(
                    id.clone(),
                    cx.spawn(async move |_, _| {
                        std::future::pending::<()>().await;
                    }),
                );
                let (key, mut state) = ready_native(&owner);
                // Prepare can first report authentication waiting and later fail.
                state.authentication = Authentication::Required;
                state.execution = None;
                state.revision = if hidden { 10 } else { 1 };
                publish(this, &key, state.clone());
                if hidden {
                    let other = this.sessions.active()[0].id.clone();
                    this.select_session(other, window, cx);
                    this.composer.update(cx, |composer, cx| {
                        composer.set_value("newer hidden-owner text", window, cx)
                    });
                }
                state.revision += 1;
                this.native_event(AgentEvent::StateChanged(state.clone()), window, cx);
                assert!(
                    this.starters.capture_pending(&id),
                    "authentication waiting retains the activation"
                );
                state.revision += 1;
                state.status = Status::Error;
                if hidden {
                    let ticket = this
                        .native_agents
                        .owner
                        .begin(key.clone(), "failed-prepare".into(), Call::Prepare, false)
                        .expect("test fixture");
                    this.finish_native_call(&ticket, None, cx);
                } else {
                    this.native_event(AgentEvent::StateChanged(state), window, cx);
                }
                assert!(
                    !this.starters.capture_pending(&id),
                    "terminal failure releases the pending recovery lock"
                );
                assert!(
                    !this.starters.capture_tasks.contains_key(&id),
                    "late overrides cannot restart activation"
                );
                expected = Some(original);
            })
        })
        .expect("window");
        cx.run_until_parked();
        let expected = expected.expect("expected value");
        cx.read(|cx| {
            let this = workspace.read(cx);
            assert!(this.starters.captures.get(&expected.id).is_none());
            assert_eq!(this.drafts.get(&expected.owner), Some(&expected.draft));
            assert!(this.creations.iter().next().is_none());
            assert_eq!(
                this.composer.read(cx).value().to_string(),
                if hidden {
                    "newer hidden-owner text"
                } else {
                    expected.draft.text.as_str()
                }
            );
        });
    }
    crate::recovery_tests::close(cx, window, &workspace);
}

#[gpui_kit::test]
fn covered_startup_retains_activation_until_each_full_page_is_closed(cx: &mut TestAppContext) {
    let (_fixture, window, workspace) = open_ready(cx);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            let project = this.topology.projects[0].clone();
            this.show_new_session(window, cx);
            this.choose_draft_project(&project.path, window, cx);
            let original = capture(
                this.composer_key.clone().expect("composer key"),
                project.id.to_string(),
                "ordinary",
            );
            let id = original.id.clone();
            this.drafts
                .save(original.owner.clone(), original.draft.clone());
            assert!(
                this.starters
                    .captures
                    .begin(original.clone(), &mut this.drafts)
            );
            this.restore_composer_draft(window, cx);
            this.starters.continuations.insert(
                id.clone(),
                StarterContinuation {
                    window: window.window_handle(),
                    rendered: Some(("visible prompt".into(), "instructions".into())),
                    effort: None,
                    native: None,
                },
            );
            for page in ["archive", "source-board", "scheduled-tasks"] {
                let toggle: fn(&mut Workspace, &mut Window, &mut Context<Workspace>) = match page {
                    "archive" => Workspace::toggle_archive_page,
                    "source-board" => Workspace::toggle_source_board,
                    _ => Workspace::toggle_scheduled_tasks,
                };
                toggle(this, window, cx);
                assert_eq!(this.composer_key.as_ref(), Some(&original.owner));
                assert!(this.selected.is_none());
                assert!(
                    !this.can_create_starter(&original),
                    "{page} covers the original view"
                );
                this.resume_starter_activation(window, cx);
                assert_eq!(this.starters.captures.get(&id), Some(&original));
                assert!(this.starters.continuations[&id].rendered.is_some());
                assert!(
                    this.creations.iter().next().is_none(),
                    "{page} cannot create behind the page"
                );
                toggle(this, window, cx);
                assert!(this.new_session_view_active());
                assert!(this.can_create_starter(&original));
            }
            this.resume_starter_activation(window, cx);
            assert!(this.starters.captures.get(&id).is_none());
            assert_eq!(
                this.creations.iter().count(),
                1,
                "returning resumes the captured activation once"
            );
            this.resume_starter_activation(window, cx);
            assert_eq!(this.creations.iter().count(), 1);
            this.validate_queue_intent_admission()
                .expect("resumed starter creation passes full journal admission");
        })
    })
    .expect("window");
    crate::recovery_tests::close(cx, window, &workspace);
}

#[gpui_kit::test]
fn newer_ready_event_survives_older_terminal_prepare_and_resumes_original_capture(
    cx: &mut TestAppContext,
) {
    use openchamber_native_core::native_agent::AgentEvent;

    for stale_status in [Status::Error, Status::Stopped] {
        let (_fixture, window, workspace) = open_ready(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                let project = this.topology.projects[0].clone();
                this.show_new_session(window, cx);
                this.choose_draft_project(&project.path, window, cx);
                let original = capture(
                    this.composer_key.clone().expect("composer key"),
                    project.id.to_string(),
                    "native-provider",
                );
                let id = original.id.clone();
                this.drafts
                    .save(original.owner.clone(), original.draft.clone());
                assert!(
                    this.starters
                        .captures
                        .begin(original.clone(), &mut this.drafts)
                );
                this.restore_composer_draft(window, cx);
                this.starters.continuations.insert(
                    id.clone(),
                    StarterContinuation {
                        window: window.window_handle(),
                        rendered: Some(("visible prompt".into(), "instructions".into())),
                        effort: None,
                        native: Some(StarterNative {
                            preferences: Some(vec![("mode".into(), "plan".into())]),
                            config: None,
                            preparation: Some(1),
                        }),
                    },
                );
                let (key, mut initial) = ready_native(&original.owner);
                initial.status = Status::Preparing;
                initial.execution = None;
                publish(this, &key, initial.clone());
                let ticket = this
                    .native_agents
                    .owner
                    .begin(key.clone(), "older-prepare".into(), Call::Prepare, false)
                    .expect("test fixture");

                let (_, mut newer) = ready_native(&original.owner);
                newer.revision = 3;
                this.native_event(AgentEvent::StateChanged(newer.clone()), window, cx);
                let frozen = this.starters.continuations[&id]
                    .native
                    .as_ref()
                    .expect("test fixture")
                    .config
                    .clone()
                    .expect("test fixture");
                let mut older = initial;
                older.revision = 2;
                older.status = stale_status;
                this.finish_native_call(
                    &ticket,
                    Some(Ok(Reply::Success {
                        request_id: ticket.request_id.clone(),
                        state: older,
                    })),
                    cx,
                );

                assert_eq!(this.native_agents.owner.state(&key), Some(&newer));
                assert!(
                    this.starters.capture_pending(&id),
                    "the stale terminal snapshot cannot restore this activation"
                );
                let retained = this.starters.captures.get(&id).expect("capture");
                assert_eq!(retained.owner, original.owner);
                assert_eq!(retained.draft, original.draft);
                assert_eq!(
                    this.starters.continuations[&id]
                        .native
                        .as_ref()
                        .expect("test fixture")
                        .config
                        .as_ref(),
                    Some(&frozen)
                );
                assert!(this.drafts.get(&original.owner).is_none());
                assert!(this.can_create_starter(retained));
                this.resume_starter_activation(window, cx);
                assert!(this.starters.captures.get(&id).is_none());
                let (_, creation) = this.creations.iter().next().expect("creation");
                assert_eq!(creation.source, original.owner);
                assert_eq!(creation.native_config.as_ref(), Some(&frozen));
                assert_eq!(creation.prompt.text, "visible prompt");
                this.validate_queue_intent_admission()
                    .expect("native starter creation passes full journal admission");
            })
        })
        .expect("window");
        crate::recovery_tests::close(cx, window, &workspace);
    }
}
